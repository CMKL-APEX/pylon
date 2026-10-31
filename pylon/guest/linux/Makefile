# SPDX-License-Identifier: GPL-2.0-only OR BSD-3-Clause
#
# Builds pv_pylon.ko against the headers of the running kernel, or of KDIR.

KDIR ?= /lib/modules/$(shell uname -r)/build

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean

.PHONY: all clean
