# SPDX-License-Identifier: Apache-2.0
#
# Copyright © 2026 CMKL-APEX

"""The agent's data-analysis tool: build a frame, group, join and sort."""
import sys

import numpy as np
import pandas as pd

rng = np.random.default_rng(int(sys.argv[1]))
n = int(sys.argv[2]) if len(sys.argv) > 2 else 1_500_000
df = pd.DataFrame({"k": rng.integers(0, 50_000, n), "g": rng.integers(0, 200, n),
                   "v": rng.random(n), "s": rng.choice(["a", "b", "c", "d"], n)})
agg = df.groupby(["g", "s"]).agg(v_mean=("v", "mean"), v_sum=("v", "sum"), n=("k", "count"))
top = df.merge(agg.reset_index(), on=["g", "s"]).nlargest(20, "v_sum")
print(agg.describe().to_string())
print(top.head(10).to_string())
