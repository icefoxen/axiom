#!/usr/bin/env fish

set successes 0
set failures 0
set runs 100

for i in (seq $runs)
    echo "Run $i, $successes success and $failures failed"
    cargo run --example goatherd
    if [ $status -eq 0 ]
        set successes (expr $successes + 1)
    else
        set failures (expr $failures + 1)
    end
end

echo
echo "DONE"
echo "$i runs, $successes successes and $failures failures"
