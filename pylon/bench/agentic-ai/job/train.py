#!/opt/venv/bin/python
# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX

"""Continuous fine-tuning load: LoRA on Qwen2.5-1.5B-Instruct over Alpaca, with
the data pipeline (chat template, augmentation, tokenization, packing) in
DataLoader workers on the CPU, as a continuous pipeline does it. Writes
train_steps.jsonl (per step: data wait, step time, tokens) and samples.jsonl."""

import argparse
import glob
import json
import os
import random
import sys
import time

import pandas as pd
import torch
from peft import LoraConfig, get_peft_model
from torch.utils.data import DataLoader, IterableDataset, get_worker_info
from transformers import AutoModelForCausalLM, AutoTokenizer

sys.path.insert(0, "/job")
from sampler import Sampler  # noqa: E402

MODEL = "/models/Qwen2.5-1.5B-Instruct"


class Packed(IterableDataset):
    """Endless stream of packed sequences of `seq_len` tokens."""

    def __init__(self, rows, seq_len, augment_passes, seed):
        self.rows, self.seq_len, self.passes, self.seed = rows, seq_len, augment_passes, seed

    def __iter__(self):
        info = get_worker_info()
        wid = info.id if info else 0
        rng = random.Random(self.seed * 997 + wid)
        tok = AutoTokenizer.from_pretrained(MODEL)
        buf = []
        while True:
            r = self.rows[rng.randrange(len(self.rows))]
            prompt = r["instruction"] + ("\n\n" + r["input"] if r["input"] else "")
            text = r["output"]
            # Augmentation, as a continuous pipeline filters and rewrites
            # its data: word dropout, re-tokenized each pass.
            for _ in range(self.passes):
                words = text.split()
                text2 = " ".join(w for w in words if rng.random() > 0.05) or text
                tok(text2)["input_ids"]
            msgs = [{"role": "user", "content": prompt}, {"role": "assistant", "content": text}]
            ids = tok.apply_chat_template(msgs, tokenize=True)
            if not isinstance(ids, list):  # newer transformers return a BatchEncoding
                ids = ids["input_ids"]
            buf.extend(ids)
            while len(buf) >= self.seq_len:
                yield torch.tensor(buf[: self.seq_len])
                buf = buf[self.seq_len:]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--params", default="/job/params.json")
    a = ap.parse_args()
    p = dict(seq_len=2048, batch=4, workers=6, augment_passes=2, lr=1e-4, seed=1,
             warmup_s=60, duration_s=300, measure_start=None)
    p.update(json.load(open(a.params)).get("trainer", {}))
    out = "/job/out"
    os.makedirs(out, exist_ok=True)
    rows = pd.concat(pd.read_parquet(f) for f in glob.glob("/models/alpaca/data/*.parquet"))
    rows = rows.to_dict("records")
    torch.manual_seed(p["seed"])
    model = AutoModelForCausalLM.from_pretrained(MODEL, torch_dtype=torch.bfloat16,
                                                 attn_implementation="sdpa").cuda()
    model.gradient_checkpointing_enable(gradient_checkpointing_kwargs={"use_reentrant": False})
    model.enable_input_require_grads()
    model = get_peft_model(model, LoraConfig(r=16, lora_alpha=32, lora_dropout=0.0,
                                             target_modules="all-linear", task_type="CAUSAL_LM"))
    model.train()  # gradient checkpointing only applies in training mode
    opt = torch.optim.AdamW([q for q in model.parameters() if q.requires_grad], lr=p["lr"])
    dl = DataLoader(Packed(rows, p["seq_len"], p["augment_passes"], p["seed"]),
                    batch_size=p["batch"], num_workers=p["workers"], pin_memory=True,
                    prefetch_factor=2, persistent_workers=True)
    sampler = Sampler(f"{out}/samples.jsonl")
    sampler.start()
    now = time.time()
    t_start = p["measure_start"] or now + p["warmup_s"]
    t_end = t_start + p["duration_s"]
    json.dump(dict(t_start=t_start, t_end=t_end, params=p, cpu_count=os.cpu_count()),
              open(f"{out}/meta.json", "w"))
    print(f"training from {now:.3f}, measuring {t_start:.3f}..{t_end:.3f}", flush=True)
    it = iter(dl)
    step = 0
    with open(f"{out}/train_steps.jsonl", "w", buffering=1) as f:
        while time.time() < t_end:
            t0 = time.time()
            batch = next(it).cuda(non_blocking=True)
            t1 = time.time()
            loss = model(input_ids=batch, labels=batch).loss
            loss.backward()
            opt.step()
            opt.zero_grad(set_to_none=True)
            torch.cuda.synchronize()
            t2 = time.time()
            f.write(json.dumps(dict(step=step, t=t0, data_wait=t1 - t0, compute=t2 - t1,
                                    tokens=batch.numel(), loss=round(loss.item(), 4),
                                    measured=t0 >= t_start)) + "\n")
            step += 1
    sampler.stop()
    print(f"done after {step} steps", flush=True)
    os._exit(0)  # don't wait for the persistent workers


if __name__ == "__main__":
    main()
