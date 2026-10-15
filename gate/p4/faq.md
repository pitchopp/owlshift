# FAQ

## What does `owlshift watch --drain` do?

`owlshift watch` continues each ticket whose questions wait once its decider's reply counts, until Ctrl-C. With `--drain`, it also takes in the admitted tickets of the ready column, intake first, and builds them one at a time.

## Who merges the pull requests?

Humans merge. Owlshift delivers a verified pull request, and agents never merge.
