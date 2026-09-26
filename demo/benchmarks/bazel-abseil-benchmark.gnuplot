# Bar chart with stddev error bars for bazel-abseil-benchmark.sh's three
# scenarios. Regenerate the .dat this reads from the benchmark's own JSON:
#
#   python3 -c "
#   import json
#   rows = json.load(open('bazel-abseil-benchmark.json'))
#   with open('bazel-abseil-benchmark.dat', 'w') as f:
#       f.write('# idx scenario mean stddev\n')
#       for i, r in enumerate(rows):
#           f.write(f'{i} \"{r[\"scenario\"]}\" {r[\"mean\"]:.4f} {r[\"stddev\"]:.4f}\n')
#   "
#   gnuplot bazel-abseil-benchmark.gnuplot

set terminal pngcairo size 900,550 enhanced font "sans,20"
set output "bazel-abseil-benchmark.png"
set size ratio 1.0
set title "abseil-cpp //...: nixception vs. vanilla Bazel build"
set ylabel "wall-clock time (s)"
set yrange [0:650]
set key top left
set xrange [-1:3]
set grid ytics
set style fill solid 0.7 border -1
set boxwidth 0.3
set xtics rotate by -15 offset -0.5,0

# column 1 = index, 2 = scenario label, 3 = mean, 4 = stddev
plot "bazel-abseil-benchmark.dat" using 1:3:4:xtic(2) with boxerrorbars \
       lc rgb "#4C72B0" lw 2 title "mean ± stddev"
