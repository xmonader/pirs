Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
When DEBUG is True, raising Http404 in a path converter's to_python method does not result in a technical response
Description
	
This is the response I get (plain text): 
A server error occurred. Please contact the administrator.
I understand a ValueError should be raised which tells the URL resolver "this path does not match, try next one" but Http404 is what came to my mind intuitively and the error message was not very helpful.
One could also make a point that raising a Http404 should be valid way to tell the resolver "this is indeed the right path but the current parameter value does not match anything so stop what you are doing and let the handler return the 404 page (including a helpful error message when DEBUG is True instead of the default 'Django tried these URL patterns')".
This would prove useful for example to implement a path converter that uses get_object_or_404.

