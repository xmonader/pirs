Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
ForeignKey's to_field parameter gets the old field's name when renaming a PrimaryKey.
Description
	
Having these two models 
class ModelA(models.Model):
	field_wrong = models.CharField('field1', max_length=50, primary_key=True) # I'm a Primary key.
class ModelB(models.Model):
	field_fk = models.ForeignKey(ModelA, blank=True, null=True, on_delete=models.CASCADE) 
... migrations applyed ...
the ModelA.field_wrong field has been renamed ... and Django recognizes the "renaming"
# Primary key renamed
class ModelA(models.Model):
	field_fixed = models.CharField('field1', max_length=50, primary_key=True) # I'm a Primary key.
Attempts to to_field parameter. 
The to_field points to the old_name (field_typo) and not to the new one ("field_fixed")
class Migration(migrations.Migration):
	dependencies = [
		('app1', '0001_initial'),
	]
	operations = [
		migrations.RenameField(
			model_name='modela',
			old_name='field_wrong',
			new_name='field_fixed',
		),
		migrations.AlterField(
			model_name='modelb',
			name='modela',
			field=models.ForeignKey(blank=True, null=True, on_delete=django.db.models.deletion.CASCADE, to='app1.ModelB', to_field='field_wrong'),
		),
	]

