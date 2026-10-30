import table


def test_bootstrap_interval_brackets_the_mean():
    assert table.interval([1.0] * 40) == (1.0, 1.0)
    lo, hi = table.interval([0.0] * 20 + [1.0] * 20)
    assert 0.3 < lo < 0.5 < hi < 0.7
