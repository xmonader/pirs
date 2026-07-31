`npm test` is broken on a fresh checkout — it dies before running anything.

Once it's running again: one of the batch handlers accepts rows with a blank label when it
should reject them. Find that one and fix it too.
