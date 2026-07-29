Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
autosummary: The members variable for module template contains imported members
**Describe the bug**
autosummary: The members variable for module template contains imported members even if autosummary_imported_members is False.

**To Reproduce**

```
# _templates/autosummary/module.rst
{{ fullname | escape | underline }}

.. automodule:: {{ fullname }}

   .. autosummary::
   {% for item in members %}
      {{ item }}
   {%- endfor %}

```
```
# example.py
import os
```
```
# index.rst
.. autosummary::
   :toctree: generated

   example
```
```
# conf.py
autosummary_generate = True
autosummary_imported_members = False
```

As a result, I got following output:
```
# generated/example.rst
example
=======

.. automodule:: example

   .. autosummary::

      __builtins__
      __cached__
      __doc__
      __file__
      __loader__
      __name__
      __package__
      __spec__
      os
```

**Expected behavior**
The template variable `members` should not contain imported members when `autosummary_imported_members` is False.

**Your project**
No

**Screenshots**
No

**Environment info**
- OS: Mac
- Python version: 3.8.2
- Sphinx version: 3.1.0dev
- Sphinx extensions:  sphinx.ext.autosummary
- Extra tools: No

**Additional context**
No


