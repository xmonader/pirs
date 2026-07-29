Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
ExpressionWrapper for ~Q(pk__in=[]) crashes.
Description
	 
		(last modified by Stefan Brand)
	 
Problem Description
I'm reducing some Q objects (similar to what is described in ticket:32554. Everything is fine for the case where the result is ExpressionWrapper(Q(pk__in=[])). However, when I reduce to ExpressionWrapper(~Q(pk__in=[])) the query breaks.
Symptoms
Working for ExpressionWrapper(Q(pk__in=[]))
print(queryset.annotate(foo=ExpressionWrapper(Q(pk__in=[]), output_field=BooleanField())).values("foo").query)
SELECT 0 AS "foo" FROM "table"
Not working for ExpressionWrapper(~Q(pk__in=[]))
print(queryset.annotate(foo=ExpressionWrapper(~Q(pk__in=[]), output_field=BooleanField())).values("foo").query)
SELECT AS "foo" FROM "table"

