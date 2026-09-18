"""Measure only eager opening, terminating probes before unrelated lookup batches."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import selectors
import statistics
import subprocess as sp


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-report', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--repetitions', type=int, default=31)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error('repetitions must be positive')
    build = json.loads(args.build_report.read_text())
    binaries = {'before': args.baseline.resolve(), 'after': args.candidate.resolve()}
    samples = {name: [] for name in binaries}
    for repetition in range(args.repetitions):
        order = list(binaries)
        random.Random(1931 + repetition).shuffle(order)
        for name in order:
            process = sp.Popen([str(binaries[name]), build['corpus']],
                               env={**os.environ, 'FXI_INDEXES': build['retained_indexes'][name]},
                               stdout=sp.PIPE, stderr=sp.DEVNULL)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(process.stdout, selectors.EVENT_READ)
                    if not selector.select(30):
                        raise TimeoutError('No eager-open measurement')
                    key, value = process.stdout.readline().decode().strip().split('\t')
                assert key == 'open_ms'
                samples[name].append(float(value))
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except sp.TimeoutExpired:
                    process.kill()
                    process.wait()
                process.stdout.close()
        print(repetition, samples['before'][-1], samples['after'][-1], flush=True)
    ratios = [after / before for before, after in zip(samples['before'], samples['after'])]
    output = {'corpus': build['corpus'], 'indexes': build['retained_indexes'],
              'binaries': {name: {'path': str(binary),
                                  'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
                           for name, binary in binaries.items()},
              'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'mode': 'warm-filesystem eager open only; probe terminated after first timing line, excluding later lookup batches; randomized pair order',
              'samples_ms': samples,
              'medians_ms': {name: statistics.median(values) for name, values in samples.items()},
              'median_paired_after_before_ratio': statistics.median(ratios)}
    args.output.write_text(json.dumps(output, indent=2) + '\n')


if __name__ == '__main__':
    main()
