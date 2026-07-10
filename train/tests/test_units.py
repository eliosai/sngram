from sngram_train.units import parse_size


def test_parse_size_accepts_decimal_and_iec_units():
    assert parse_size("16MB") == 16_000_000
    assert parse_size("16 MiB") == 16 * 2**20
