#!/usr/bin/env python3
import csv, math, random, os

random.seed(20261022)
strains = ["WT", "dA", "dB", "dC", "dD", "dE"]
treatments = ["control", "drugA_low", "drugA_high", "drugB"]
untreated_rate_per_hour = {"WT": 0.62, "dA": 0.55, "dB": 0.48, "dC": 0.66, "dD": 0.40, "dE": 0.58}
treatment_scale = {"control": 1.0, "drugA_low": 0.8, "drugA_high": 0.55, "drugB": 1.0}
drugB_scale_by_strain = {"dB": 0.5, "dD": 0.6}
capacity = {"WT": 1.2, "dA": 1.1, "dB": 0.9, "dC": 1.25, "dD": 0.8, "dE": 1.0}
rows = "ABCDEFGH"
NEVER_INOCULATED = "C07"
CONTAMINATED_LATE = "E10"
SATURATED = "B03"
READER_CEILING = "4.0000"
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
        r = untreated_rate_per_hour[s] * treatment_scale[t] * (drugB_scale_by_strain.get(s, 1.0) if t == "drugB" else 1.0) * random.gauss(1, 0.05)
        K = capacity[s] * (0.85 if t == "drugA_high" else 1.0) * random.gauss(1, 0.03)
        lag = 2.0 + (1.5 if t == "drugA_high" else 0.0) + random.gauss(0, 0.3)
        N0 = 0.005
        for time in times:
            if well == NEVER_INOCULATED:
                od = 0.04 + random.gauss(0, 0.002)
            else:
                x = max(time - lag, 0)
                n = K / (1 + (K / N0 - 1) * math.exp(-r * x))
                if well == CONTAMINATED_LATE and time > 14:
                    n += 0.08 * (time - 14)
                od = 0.04 + n + random.gauss(0, 0.004 + 0.01 * n)
            temp = 37 + random.gauss(0, 0.1)
            cell = f"{od:.4f}"
            if random.random() < 0.004:
                cell = ""
            if well == SATURATED and time == 12.0:
                cell = READER_CEILING
            w.writerow([well, s, t, rep, time, cell, f"{temp:.2f}"])
print(out)
