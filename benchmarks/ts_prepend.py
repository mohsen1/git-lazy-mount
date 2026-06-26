import sys, time
start = time.time()
for line in sys.stdin:
    sys.stdout.write(f"{time.time()-start:.1f}\t{line}")
