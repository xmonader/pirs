Fix the following issue in this repository at /testbed.

Rules:
- Investigate with tools; find the true root cause before editing.
- Fix SOURCE code only. Do not edit, delete, or weaken tests.
- You may run existing project tests if helpful, but do not rely on tests that are not in the tree.
- Make the minimal correct fix; stop when you believe the issue is resolved.

## Issue
Customizable management command formatters.
Description
	
With code like:
class Command(BaseCommand):
	help = '''
	Import a contract from tzkt.
	Example usage:
		./manage.py tzkt_import 'Tezos Mainnet' KT1HTDtMBRCKoNHjfWEEvXneGQpCfPAt6BRe
	'''
Help output is:
$ ./manage.py help tzkt_import
usage: manage.py tzkt_import [-h] [--api API] [--version] [-v {0,1,2,3}] [--settings SETTINGS]
							 [--pythonpath PYTHONPATH] [--traceback] [--no-color] [--force-color]
							 [--skip-checks]
							 blockchain target
Import a contract from tzkt Example usage: ./manage.py tzkt_import 'Tezos Mainnet'
KT1HTDtMBRCKoNHjfWEEvXneGQpCfPAt6BRe
positional arguments:
 blockchain			Name of the blockchain to import into
 target				Id of the contract to import
When that was expected:
$ ./manage.py help tzkt_import
usage: manage.py tzkt_import [-h] [--api API] [--version] [-v {0,1,2,3}] [--settings SETTINGS]
							 [--pythonpath PYTHONPATH] [--traceback] [--no-color] [--force-color]
							 [--skip-checks]
							 blockchain target
Import a contract from tzkt 
Example usage: 
	./manage.py tzkt_import 'Tezos Mainnet' KT1HTDtMBRCKoNHjfWEEvXneGQpCfPAt6BRe
positional arguments:
 blockchain			Name of the blockchain to import into
 target				Id of the contract to import

