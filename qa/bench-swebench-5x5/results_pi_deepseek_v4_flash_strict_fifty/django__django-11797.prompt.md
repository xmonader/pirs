Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
Filtering on query result overrides GROUP BY of internal query
Description
	
from django.contrib.auth import models
a = models.User.objects.filter(email__isnull=True).values('email').annotate(m=Max('id')).values('m')
print(a.query) # good
# SELECT MAX("auth_user"."id") AS "m" FROM "auth_user" WHERE "auth_user"."email" IS NULL GROUP BY "auth_user"."email"
print(a[:1].query) # good
# SELECT MAX("auth_user"."id") AS "m" FROM "auth_user" WHERE "auth_user"."email" IS NULL GROUP BY "auth_user"."email" LIMIT 1
b = models.User.objects.filter(id=a[:1])
print(b.query) # GROUP BY U0."id" should be GROUP BY U0."email"
# SELECT ... FROM "auth_user" WHERE "auth_user"."id" = (SELECT U0."id" FROM "auth_user" U0 WHERE U0."email" IS NULL GROUP BY U0."id" LIMIT 1)

