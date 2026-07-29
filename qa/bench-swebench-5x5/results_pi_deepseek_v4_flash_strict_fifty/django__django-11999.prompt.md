Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
Cannot override get_FOO_display() in Django 2.2+.
Description
	
I cannot override the get_FIELD_display function on models since version 2.2. It works in version 2.1.
Example:
class FooBar(models.Model):
	foo_bar = models.CharField(_("foo"), choices=[(1, 'foo'), (2, 'bar')])
	def __str__(self):
		return self.get_foo_bar_display() # This returns 'foo' or 'bar' in 2.2, but 'something' in 2.1
	def get_foo_bar_display(self):
		return "something"
What I expect is that I should be able to override this function.

