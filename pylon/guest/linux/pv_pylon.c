// SPDX-License-Identifier: GPL-2.0-only OR BSD-3-Clause
/*
 * Copyright © 2026 CMKL-APEX
 *
 * pv-pylon: the guest side of Pylon's kick and park path
 * (spec/06-guest-kick-path.md §4.3 and §4.6).
 *
 * When pylond wants a core back from vCPU i, it sets revoke_requested in
 * vCPU i's slot of the shared page and kicks MSI-X vector i of the pylon-kick
 * device. The kick lands on CPU i. The handler only wakes CPU i's park thread,
 * a SCHED_FIFO per-CPU kthread. Once that thread runs, the CPU is at a safe
 * point: the interrupted task was preempted with no spinlock held and
 * preemption enabled. The thread writes state = PARKING and then the park
 * doorbell in BAR0. The doorbell write exits to the VMM, which parks the vCPU,
 * and the write completes only when pylond wakes it.
 *
 * Linux CPU i must be vCPU i. Cloud Hypervisor lists vCPUs in order in the
 * MADT (x86_64) and the device tree or MADT (aarch64), so this holds unless
 * the guest renumbers CPUs.
 *
 * Vector i must interrupt CPU i, so the vectors aren't managed: the kernel's
 * spreading doesn't keep that order (Linux 7.0 puts vector 0 on CPU 1 of a
 * two-CPU guest). Each vector is pinned to its CPU whenever that CPU comes
 * online, and is kept away from irqbalance.
 *
 * The module also gives userspace the shared page as /dev/pylon. The guest
 * agent can't map it through /dev/mem while this module has it mapped: x86
 * refuses the agent's uncached mapping of a page the kernel maps write-back.
 */

#define pr_fmt(fmt) KBUILD_MODNAME ": " fmt

#include <linux/bitops.h>
#include <linux/cpuhotplug.h>
#include <linux/cpumask.h>
#include <linux/interrupt.h>
#include <linux/io.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/pci.h>
#include <linux/percpu.h>
#include <linux/sched.h>
#include <linux/sched/stat.h>
#include <linux/smpboot.h>

#include "pv_pylon_abi.h"

#if BITS_PER_LONG != 64
#error "pv-pylon needs 64-bit longs for the stranded work bitmap"
#endif

struct pv_pylon_stats {
	/* Kicks taken. */
	u64 kicks;
	/* Kicks with no revocation pending. */
	u64 spurious;
	/* Kicks that arrived on another CPU, and were passed on to theirs. */
	u64 forwarded;
	/* Park doorbells written. */
	u64 parks;
	/* Wakeups of the park thread that found no revocation pending. */
	u64 stale;
};

struct pv_pylon {
	struct pci_dev *pdev;
	void __iomem *regs;
	struct pylon_shared_page *page;
	unsigned int nr_vectors;
	enum cpuhp_state online_state;
};

/* Set before /dev/pylon exists and never changed: the VMM fixes the page. */
static phys_addr_t pv_pylon_page_gpa;

/* There is one pylon-kick device per VM, and the park threads are global. */
static struct pv_pylon *pv_pylon;

static DEFINE_PER_CPU(struct task_struct *, park_task);
static DEFINE_PER_CPU(bool, park_pending);
static DEFINE_PER_CPU(struct pv_pylon_stats, stats);

static irqreturn_t pv_pylon_kick(int irq, void *data)
{
	unsigned int vcpu = (unsigned long)data;
	struct pylon_vcpu_slot *slot = &pv_pylon->page->vcpus[vcpu];

	this_cpu_inc(stats.kicks);
	/* Pairs with pylond's release store of revoke_requested. */
	if (!smp_load_acquire(&slot->revoke_requested)) {
		this_cpu_inc(stats.spurious);
		return IRQ_HANDLED;
	}

	/*
	 * A kick can land on another CPU: x86 without interrupt remapping
	 * moves an MSI to its new CPU only at its next interrupt, and a vector
	 * whose CPU is offline is moved away. Waking the park thread of an
	 * online CPU sends it an IPI; the thread of an offline CPU is parked
	 * and stays so.
	 */
	if (vcpu != smp_processor_id())
		this_cpu_inc(stats.forwarded);
	WRITE_ONCE(per_cpu(park_pending, vcpu), true);
	wake_up_process(per_cpu(park_task, vcpu));
	return IRQ_HANDLED;
}

static int pv_pylon_park_should_run(unsigned int cpu)
{
	return READ_ONCE(*this_cpu_ptr(&park_pending));
}

static void pv_pylon_park(unsigned int cpu)
{
	struct pylon_shared_page *page = pv_pylon->page;
	unsigned long *stranded =
		(unsigned long *)&page->telemetry.stranded_work_bitmap;
	struct pylon_vcpu_slot *slot = &page->vcpus[cpu];
	bool has_waiters;

	WRITE_ONCE(*this_cpu_ptr(&park_pending), false);

	/* The VMM clears the request when it parks the vCPU for any reason. */
	if (!smp_load_acquire(&slot->revoke_requested)) {
		this_cpu_inc(stats.stale);
		return;
	}

	/* This thread is one of the runnable tasks; any other is stranded. */
	has_waiters = !single_task_running();
	if (has_waiters)
		set_bit(cpu, stranded);

	/*
	 * Stay on this CPU between the two writes: PARKING is a statement
	 * about the vCPU that writes it, and the doorbell parks the vCPU that
	 * writes it.
	 */
	preempt_disable();
	/* Publishes the stranded work bit before the VMM sees PARKING. */
	smp_store_release(&slot->state, VCPU_STATE_PARKING);
	writel(1, pv_pylon->regs + PYLON_KICK_REG_PARK_DOORBELL);
	preempt_enable();
	this_cpu_inc(stats.parks);

	/* Woken up: this CPU runs its own work again. */
	if (has_waiters)
		clear_bit(cpu, stranded);
}

static void pv_pylon_park_setup(unsigned int cpu)
{
	sched_set_fifo(current);
}

static struct smp_hotplug_thread pv_pylon_park_threads = {
	.store			= &park_task,
	.thread_should_run	= pv_pylon_park_should_run,
	.thread_fn		= pv_pylon_park,
	.thread_comm		= "pylon_park/%u",
	.setup			= pv_pylon_park_setup,
};

static ssize_t stats_show(struct device *dev, struct device_attribute *attr,
			  char *buf)
{
	struct pv_pylon_stats sum = {};
	unsigned int cpu;

	for_each_possible_cpu(cpu) {
		struct pv_pylon_stats *s = per_cpu_ptr(&stats, cpu);

		sum.kicks += READ_ONCE(s->kicks);
		sum.spurious += READ_ONCE(s->spurious);
		sum.forwarded += READ_ONCE(s->forwarded);
		sum.parks += READ_ONCE(s->parks);
		sum.stale += READ_ONCE(s->stale);
	}
	return sysfs_emit(buf,
			  "kicks %llu\nspurious %llu\nforwarded %llu\nparks %llu\nstale %llu\n",
			  sum.kicks, sum.spurious, sum.forwarded, sum.parks,
			  sum.stale);
}
static DEVICE_ATTR_RO(stats);

static struct attribute *pv_pylon_attrs[] = {
	&dev_attr_stats.attr,
	NULL,
};
ATTRIBUTE_GROUPS(pv_pylon);

/*
 * Maps the shared page with the default page protection, which on x86 takes
 * the write-back type the kernel's mapping already reserved.
 */
static int pv_pylon_mmap(struct file *file, struct vm_area_struct *vma)
{
	unsigned long size = vma->vm_end - vma->vm_start;

	if (vma->vm_pgoff || size > PYLON_SHARED_PAGE_SIZE ||
	    !(vma->vm_flags & VM_SHARED))
		return -EINVAL;
	vm_flags_set(vma, VM_IO | VM_DONTEXPAND | VM_DONTDUMP);
	return remap_pfn_range(vma, vma->vm_start,
			       PHYS_PFN(pv_pylon_page_gpa), size,
			       vma->vm_page_prot);
}

static const struct file_operations pv_pylon_fops = {
	.owner	= THIS_MODULE,
	.mmap	= pv_pylon_mmap,
};

static struct miscdevice pv_pylon_misc = {
	.minor	= MISC_DYNAMIC_MINOR,
	.name	= "pylon",
	.fops	= &pv_pylon_fops,
};

static void pv_pylon_free_irqs(struct pv_pylon *pylon, unsigned int count)
{
	unsigned int i;

	for (i = 0; i < count; i++)
		free_irq(pci_irq_vector(pylon->pdev, i),
			 (void *)(unsigned long)i);
}

static int pv_pylon_request_irqs(struct pv_pylon *pylon)
{
	struct pci_dev *pdev = pylon->pdev;
	unsigned int i;
	int ret;

	for (i = 0; i < pylon->nr_vectors; i++) {
		ret = request_irq(pci_irq_vector(pdev, i), pv_pylon_kick,
				  IRQF_NOBALANCING, "pylon-kick",
				  (void *)(unsigned long)i);
		if (ret) {
			pv_pylon_free_irqs(pylon, i);
			return ret;
		}
	}
	return 0;
}

/*
 * Runs on every CPU that is online when the driver binds, and on every CPU
 * that comes online later. While a CPU is offline, the kernel moves its vector
 * to another CPU.
 */
static int pv_pylon_cpu_online(unsigned int cpu)
{
	struct pv_pylon *pylon = pv_pylon;
	int ret;

	if (cpu >= pylon->nr_vectors)
		return 0;
	ret = irq_set_affinity(pci_irq_vector(pylon->pdev, cpu),
			       cpumask_of(cpu));
	if (ret)
		dev_warn(&pylon->pdev->dev,
			 "can't pin vector %u to its CPU: %d\n", cpu, ret);
	return 0;
}

static int pv_pylon_check_page(struct pv_pylon *pylon, u32 num_vcpus)
{
	struct pylon_header *header = &pylon->page->header;
	u64 magic = READ_ONCE(header->magic);
	u32 version = READ_ONCE(header->version);

	if (magic != PYLON_MAGIC || version != PYLON_ABI_VERSION) {
		dev_err(&pylon->pdev->dev,
			"shared page has magic %#llx, ABI version %u\n",
			magic, version);
		return -ENODEV;
	}
	if (READ_ONCE(header->num_vcpus) != num_vcpus) {
		dev_err(&pylon->pdev->dev,
			"shared page has %u vCPUs, the device %u\n",
			READ_ONCE(header->num_vcpus), num_vcpus);
		return -ENODEV;
	}
	return 0;
}

static int pv_pylon_probe(struct pci_dev *pdev, const struct pci_device_id *id)
{
	struct pv_pylon *pylon;
	u32 magic, version, num_vcpus;
	phys_addr_t gpa;
	int ret;

	if (pv_pylon) {
		dev_err(&pdev->dev, "only one pylon-kick device is supported\n");
		return -EBUSY;
	}

	pylon = kzalloc(sizeof(*pylon), GFP_KERNEL);
	if (!pylon)
		return -ENOMEM;
	pylon->pdev = pdev;

	ret = pci_enable_device_mem(pdev);
	if (ret)
		goto err_free;
	ret = pci_request_region(pdev, 0, KBUILD_MODNAME);
	if (ret)
		goto err_disable;
	pylon->regs = pci_iomap(pdev, 0, 0);
	if (!pylon->regs) {
		ret = -ENOMEM;
		goto err_release;
	}

	magic = ioread32(pylon->regs + PYLON_KICK_REG_MAGIC);
	version = ioread32(pylon->regs + PYLON_KICK_REG_ABI_VERSION);
	num_vcpus = ioread32(pylon->regs + PYLON_KICK_REG_NUM_VCPUS);
	if (magic != (u32)PYLON_MAGIC || version != PYLON_ABI_VERSION) {
		dev_err(&pdev->dev, "unknown device: magic %#x, ABI version %u\n",
			magic, version);
		ret = -ENODEV;
		goto err_unmap_regs;
	}
	if (!num_vcpus || num_vcpus > PYLON_MAX_VCPUS) {
		dev_err(&pdev->dev, "bad vCPU count %u\n", num_vcpus);
		ret = -ENODEV;
		goto err_unmap_regs;
	}

	gpa = ioread32(pylon->regs + PYLON_KICK_REG_SHM_GPA_LO) |
	      (u64)ioread32(pylon->regs + PYLON_KICK_REG_SHM_GPA_HI) << 32;
	pylon->page = memremap(gpa, PYLON_SHARED_PAGE_SIZE, MEMREMAP_WB);
	if (!pylon->page) {
		dev_err(&pdev->dev, "can't map the shared page at %pa\n", &gpa);
		ret = -ENOMEM;
		goto err_unmap_regs;
	}
	ret = pv_pylon_check_page(pylon, num_vcpus);
	if (ret)
		goto err_unmap_page;

	/*
	 * One vector per CPU that has a slot. CPUs past the last vector are
	 * never kicked; the VMM force-parks them when a revocation expires.
	 */
	pylon->nr_vectors = min(num_vcpus, nr_cpu_ids);
	if (pylon->nr_vectors < num_vcpus)
		dev_warn(&pdev->dev,
			 "%u of %u vCPUs are possible CPUs; the rest can't park cooperatively\n",
			 pylon->nr_vectors, num_vcpus);

	pci_set_master(pdev);
	ret = pci_alloc_irq_vectors(pdev, pylon->nr_vectors, pylon->nr_vectors,
				    PCI_IRQ_MSIX);
	if (ret < 0)
		goto err_clear_master;

	/* The kick handler uses both, so set them before any kick arrives. */
	pv_pylon = pylon;
	ret = smpboot_register_percpu_thread(&pv_pylon_park_threads);
	if (ret)
		goto err_free_vectors;
	ret = pv_pylon_request_irqs(pylon);
	if (ret)
		goto err_unregister_threads;
	ret = cpuhp_setup_state(CPUHP_AP_ONLINE_DYN, "pylon/kick:online",
				pv_pylon_cpu_online, NULL);
	if (ret < 0)
		goto err_free_irqs;
	pylon->online_state = ret;
	pv_pylon_page_gpa = gpa;
	ret = misc_register(&pv_pylon_misc);
	if (ret)
		goto err_remove_state;

	pci_set_drvdata(pdev, pylon);
	dev_info(&pdev->dev, "%u kick vectors, shared page at %pa\n",
		 pylon->nr_vectors, &gpa);
	return 0;

err_remove_state:
	cpuhp_remove_state(pylon->online_state);
err_free_irqs:
	pv_pylon_free_irqs(pylon, pylon->nr_vectors);
err_unregister_threads:
	smpboot_unregister_percpu_thread(&pv_pylon_park_threads);
err_free_vectors:
	pv_pylon = NULL;
	pci_free_irq_vectors(pdev);
err_clear_master:
	pci_clear_master(pdev);
err_unmap_page:
	memunmap(pylon->page);
err_unmap_regs:
	pci_iounmap(pdev, pylon->regs);
err_release:
	pci_release_region(pdev, 0);
err_disable:
	pci_disable_device(pdev);
err_free:
	kfree(pylon);
	return ret;
}

static void pv_pylon_remove(struct pci_dev *pdev)
{
	struct pv_pylon *pylon = pci_get_drvdata(pdev);

	misc_deregister(&pv_pylon_misc);
	cpuhp_remove_state(pylon->online_state);
	pv_pylon_free_irqs(pylon, pylon->nr_vectors);
	smpboot_unregister_percpu_thread(&pv_pylon_park_threads);
	pv_pylon = NULL;
	pci_free_irq_vectors(pdev);
	pci_clear_master(pdev);
	memunmap(pylon->page);
	pci_iounmap(pdev, pylon->regs);
	pci_release_region(pdev, 0);
	pci_disable_device(pdev);
	kfree(pylon);
}

static const struct pci_device_id pv_pylon_ids[] = {
	{ PCI_DEVICE(PYLON_KICK_VENDOR_ID, PYLON_KICK_DEVICE_ID) },
	{}
};
MODULE_DEVICE_TABLE(pci, pv_pylon_ids);

static struct pci_driver pv_pylon_driver = {
	.name		= KBUILD_MODNAME,
	.id_table	= pv_pylon_ids,
	.probe		= pv_pylon_probe,
	.remove		= pv_pylon_remove,
	.dev_groups	= pv_pylon_groups,
};
module_pci_driver(pv_pylon_driver);

MODULE_DESCRIPTION("Pylon guest kick and cooperative park");
MODULE_AUTHOR("CMKL-APEX");
MODULE_LICENSE("Dual BSD/GPL");
