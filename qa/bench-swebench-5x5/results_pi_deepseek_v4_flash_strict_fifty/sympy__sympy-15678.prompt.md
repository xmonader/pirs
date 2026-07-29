Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
Some issues with idiff
idiff doesn't support Eq, and it also doesn't support f(x) instead of y. Both should be easy to correct.

```
>>> idiff(Eq(y*exp(y), x*exp(x)), y, x)
Traceback (most recent call last):
  File "<stdin>", line 1, in <module>
  File "./sympy/geometry/util.py", line 582, in idiff
    yp = solve(eq.diff(x), dydx)[0].subs(derivs)
IndexError: list index out of range
>>> idiff(f(x)*exp(f(x)) - x*exp(x), f(x), x)
Traceback (most recent call last):
  File "<stdin>", line 1, in <module>
  File "./sympy/geometry/util.py", line 574, in idiff
    raise ValueError("expecting x-dependent symbol(s) but got: %s" % y)
ValueError: expecting x-dependent symbol(s) but got: f(x)
>>> idiff(y*exp(y)- x*exp(x), y, x)
(x + 1)*exp(x - y)/(y + 1)
```

