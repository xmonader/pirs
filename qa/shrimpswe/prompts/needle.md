One of the batch handlers is letting through rows it shouldn't.

Support has a batch where a row with a blank label got accepted and ended up on a
customer-facing report as an empty line. Every handler is supposed to reject rows whose
label is blank. Most of them do.

Find the one that doesn't and fix it.
