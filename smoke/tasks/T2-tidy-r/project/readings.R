### An Ember notebook ###
# /// environment
# ///

# %% id=S [markdown]
#' # Sensor readings
#'
#' **Summary:** the mean of the cleaned readings is 4.2.

# %% id=P
sprintf("Cleaned mean: %.2f, from %d readings.", clean_mean, length(clean))

# %% id=D
readings <- c(4.8, 5.2, 5.2, 4.9, 25.0, 5.4)

# %% id=F
first_try <- mean(readings)

# %% id=X
scratch <- 1 + 1

# %% id=C
clean <- readings[readings < 20]

# %% id=M
clean_mean <- mean(clean)

# /// cell order
# S
# P
# D
# F
# X
# C
# M
# ///
