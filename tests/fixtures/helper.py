def moving_average(values, window):
    """Return the moving average of a list of numbers."""
    if window <= 0:
        raise ValueError("window must be positive")
    out = []
    for i in range(len(values) - window + 1):
        out.append(sum(values[i:i + window]) / window)
    return out


if __name__ == "__main__":
    print(moving_average([1, 2, 3, 4, 5, 6], 3))
