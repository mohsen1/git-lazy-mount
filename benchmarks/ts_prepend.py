import sys, time
start = time.monotonic()
for line in sys.stdin:
    sys.stdout.write(f"{time.monotonic()-start:.1f}\t{line}")
