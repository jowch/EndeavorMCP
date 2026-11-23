### An Ember notebook ###
# /// environment
# ///

# %% id=L
library(emberui)

# %% id=C
cutoff <- input_slider(1, 5, value = 2.5, step = 0.5, label = "Yield cutoff (t/ha)")

# %% id=Y
yields <- c(1.2, 2.0, 2.8, 3.1, 3.6, 4.2, 4.5, 5.0)

# %% id=A
above <- yields[yields > cutoff]

# %% id=M
sprintf("Mean yield above the cutoff: %.2f t/ha, from %d plots", mean(above), length(above))

# /// cell order
# L
# C
# Y
# A
# M
# ///
