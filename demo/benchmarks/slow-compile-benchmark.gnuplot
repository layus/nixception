# Bar chart with stddev error bars for slow-compile-benchmark.sh's five
# scenarios. Regenerate the .dat this reads from the benchmark's own JSON:
#
#   python3 -c "
#   import json
#   rows = json.load(open('slow-compile-benchmark.json'))
#   with open('slow-compile-benchmark.dat', 'w') as f:
#       f.write('# idx scenario mean stddev\n')
#       for i, r in enumerate(rows):
#           f.write(f'{i} \"{r[\"scenario\"]}\" {r[\"mean\"]:.4f} {r[\"stddev\"]:.4f}\n')
#   "
#   gnuplot slow-compile-benchmark.gnuplot

set terminal pngcairo size 900,550 enhanced font "sans,20"
set output "slow-compile-benchmark.png"
#set size ratio 1.0
set title "slow-compile: nixception cache payoff"
set ylabel "wall-clock time (s)"
set yrange [0:12]
set xrange [-1:5]
set grid ytics
set style fill solid 0.7 border -1
set boxwidth 0.3
set xtics rotate by -15 offset -0.5,0

# column 1 = index, 2 = scenario label, 3 = mean, 4 = stddev
plot "slow-compile-benchmark.dat" using 1:3:4:xtic(2) with boxerrorbars \
       lc rgb "#4C72B0" lw 2 title "mean ± stddev"
