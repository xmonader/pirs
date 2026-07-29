Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
Add support for SCRIPT_NAME in STATIC_URL and MEDIA_URL
Description
	 
		(last modified by Rostyslav Bryzgunov)
	 
By default, {% static '...' %} tag just appends STATIC_URL in the path. When running on sub-path, using SCRIPT_NAME WSGI param, it results in incorrect static URL - it doesn't prepend SCRIPT_NAME prefix.
This problem can be solved with prepending SCRIPT_NAME to STATIC_URL in settings.py but that doesn't work when SCRIPT_NAME is a dynamic value.
This can be easily added into default Django static tag and django.contrib.staticfiles tag as following:
def render(self, context):
	url = self.url(context)
	# Updating url here with request.META['SCRIPT_NAME'] 
	if self.varname is None:
		return url
	context[self.varname] = url
		return ''
On more research I found that FileSystemStorage and StaticFilesStorage ignores SCRIPT_NAME as well. 
We might have to do a lot of changes but I think it's worth the efforts.

