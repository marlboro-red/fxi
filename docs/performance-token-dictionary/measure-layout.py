"""Measure lossless token dictionary layouts without modifying an index.
Usage: python3 measure-layout.py INDEX_ROOT [INDEX_ROOT ...]
Input must contain legacy fixed-width dictionaries with token positions.
"""
import json
import pathlib
import struct
import sys

def varint_bytes(value):
    return max(1, (value.bit_length() + 6) // 7)

for argument in sys.argv[1:]:
    root = pathlib.Path(argument)
    result = dict(root=str(root), dictionaries=0, entries=0, legacy_bytes=0,
                  raw_token_bytes=0, prefix_token_bytes=0,
                  compact_absolute_bytes=0, prefix_implicit_bytes=0)
    for path in root.rglob('tokens.dict'):
        data = path.read_bytes()
        count, = struct.unpack_from('<I', data)
        assert count != 0xffffffff, 'expected legacy dictionaries'
        cursor = 4
        previous = b''
        expected_offset = expected_position = 0
        result['dictionaries'] += 1
        result['entries'] += count
        result['legacy_bytes'] += len(data)
        result['compact_absolute_bytes'] += 12
        for _ in range(count):
            length, = struct.unpack_from('<H', data, cursor)
            cursor += 2
            token = data[cursor:cursor + length]
            cursor += length
            offset, size, frequency, position, position_size = struct.unpack_from('<QIIQI', data, cursor)
            cursor += 28
            common = next((i for i, (a, b) in enumerate(zip(previous, token)) if a != b), min(len(previous), length))
            prefix_size = varint_bytes(common) + varint_bytes(length - common) + length - common
            result['raw_token_bytes'] += length
            result['prefix_token_bytes'] += prefix_size
            result['compact_absolute_bytes'] += 2 + length + sum(map(varint_bytes, (offset, size, frequency, position, position_size)))
            result['prefix_implicit_bytes'] += prefix_size + sum(map(varint_bytes, (size, frequency, position_size)))
            assert offset == expected_offset
            assert position == expected_position or position_size == 0
            expected_offset += size
            expected_position += position_size
            previous = token
        assert cursor == len(data)
    print(json.dumps(result, sort_keys=True))
