#!/usr/bin/env python3
"""Writes project/growth.csv, the L1 task's plate: 96 wells, OD600 every 30 min for 24 h.

Six strains, four treatments, four replicates. The plate has the faults a real
one has: some readings missing, one well contaminated (a second rise late),
one reading saturated at the reader's ceiling, and one well never inoculated.
Run it again only to change the data; the checks depend on it.
"""
import csv, math, random, os

random.seed(20261022)
strains = ["WT", "dA", "dB", "dC", "dD", "dE"]
treatments = ["control", "drugA_low", "drugA_high", "drugB"]
# Growth rate (per hour) of each strain untreated, and how each treatment scales it.
rate = {"WT": 0.62, "dA": 0.55, "dB": 0.48, "dC": 0.66, "dD": 0.40, "dE": 0.58}
scale = {"control": 1.0, "drugA_low": 0.8, "drugA_high": 0.55, "drugB": 1.0}
# drugB acts only on two strains.
drugB = {"dB": 0.5, "dD": 0.6}
capacity = {"WT": 1.2, "dA": 1.1, "dB": 0.9, "dC": 1.25, "dD": 0.8, "dE": 1.0}
rows = "ABCDEFGH"
times = [i * 0.5 for i in range(49)]

wells = []
for s_i, s in enumerate(strains):
    for t_i, t in enumerate(treatments):
        for rep in range(1, 5):
            k = (s_i * 4 + t_i) * 4 + rep - 1
            wells.append((f"{rows[k // 12]}{k % 12 + 1:02d}", s, t, rep))

out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "project", "growth.csv")
with open(out, "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["well", "strain", "treatment", "replicate", "time_h", "od600", "temp_c"])
    for well, s, t, rep in wells:
        r = rate[s] * scale[t] * (drugB.get(s, 1.0) if t == "drugB" else 1.0) * random.gauss(1, 0.05)
        K = capacity[s] * (0.85 if t == "drugA_high" else 1.0) * random.gauss(1, 0.03)
        lag = 2.0 + (1.5 if t == "drugA_high" else 0.0) + random.gauss(0, 0.3)
        N0 = 0.005
        for time in times:
            if well == "C07":  # never inoculated
                od = 0.04 + random.gauss(0, 0.002)
            else:
                x = max(time - lag, 0)
                n = K / (1 + (K / N0 - 1) * math.exp(-r * x))
                if well == "E10" and time > 14:  # contaminated: a second rise late
                    n += 0.08 * (time - 14)
                od = 0.04 + n + random.gauss(0, 0.004 + 0.01 * n)
            temp = 37 + random.gauss(0, 0.1)
            cell = f"{od:.4f}"
            if random.random() < 0.004:
                cell = ""
            if well == "B03" and time == 12.0:
                cell = "4.0000"  # the reader's ceiling
            w.writerow([well, s, t, rep, time, cell, f"{temp:.2f}"])
print(out)
