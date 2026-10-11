# Trusted node-authored pre-capture action; executes only in a fresh builder VM.
import hashlib,json,os,shutil,stat,subprocess,sys
from pathlib import Path
from urllib.parse import urlsplit,urlunsplit

try:
 metadata=json.loads(sys.argv[1]); run_id=sys.argv[2]; compact=run_id.replace('-','')
 workspace=Path('/workspace/project')
 assert workspace.resolve(strict=True)==workspace and workspace.is_dir()
 assert (workspace/'.git').is_dir() and not (workspace/'.git').is_symlink()
 boot=Path('/proc/sys/kernel/random/boot_id').read_text().strip()
 state=Path('/run/ahvm-project-environment')
 info=state.lstat(); assert stat.S_ISDIR(info.st_mode) and info.st_uid==0 and info.st_mode&0o077==0
 receipt_path=state/(compact+'.install.result.json'); info=receipt_path.lstat()
 assert stat.S_ISREG(info.st_mode) and info.st_uid==0 and info.st_mode&0o077==0 and info.st_size<=4096
 receipt=json.loads(receipt_path.read_text())
 expected={'schema':1,'run_id':compact,'workspace_id':hashlib.sha256(str(workspace).encode()).hexdigest(),'workspace_path':str(workspace),'revision_id':metadata['revision_id'],'boot_id':boot,'hook':'install','hook_sha256':metadata['install_hook_digest'],'exit_code':0,'empty':True,'cancelled':False,'timed_out':False}
 assert all(receipt.get(key)==value for key,value in expected.items())
 assert not (state/(compact+'.start.started.json')).exists()
 assert not (state/(expected['workspace_id']+'.service.json')).exists()
 assert not (Path('/run/ahvm-managed-cancel')/run_id).exists()
 run_dir=Path('/home/ahvm/.local/state/ahvm-agent/runs')/run_id
 source=run_dir/'input.json'; assert source.stat().st_size<=65536
 request=json.loads(source.read_text())
 assert request.get('mode')=='environment' and request.get('harness')=='environment'
 assert request.get('preparationMode')=='install_only' and request.get('runId')==run_id
 context=request['projectEnvironment']
 assert context['projectId']==metadata['project_id'] and context['revisionId']==metadata['revision_id']
 assert context['sourceCommit']==metadata['source_commit']
 assert context.get('baseImageDigest')==metadata['base_image_digest']
 assert context.get('baseContentDigest')==metadata['base_content_digest']
 assert context.get('installDigest')==metadata['install_hook_digest']
 assert context.get('startDigest')==metadata['start_hook_digest']
 assert request.get('workspaceDirectory')==str(workspace)
 assert not any(key in request for key in ('provider','model','prompt','sessionId','messageId','text'))
 git=['/usr/bin/git','-c','safe.directory='+str(workspace),'-C',str(workspace)]
 head=subprocess.run(git+['rev-parse','HEAD'],check=True,capture_output=True,text=True,timeout=10).stdout.strip()
 assert head==metadata['source_commit']
 # Remove credential helpers/headers and embedded URL authentication without
 # deleting project dependencies, generated files, or the canonical checkout.
 config=subprocess.run(git+['config','--local','--list'],check=True,capture_output=True,text=True,timeout=10).stdout
 assert len(config.encode())<=65536
 for line in config.splitlines():
  key,_,value=line.partition('='); lower=key.lower()
  if lower.startswith('credential.') or lower=='core.askpass' or (lower.startswith('http.') and lower.endswith('.extraheader')):
   subprocess.run(git+['config','--local','--unset-all',key],capture_output=True,timeout=10)
  elif lower.startswith('remote.') and lower.endswith('.url'):
   url=urlsplit(value)
   if url.username is not None or url.password is not None:
    assert url.scheme=='https' and url.hostname=='github.com'
    clean=urlunsplit((url.scheme,url.hostname,url.path,'',''))
    subprocess.run(git+['config','--local','--replace-all',key,clean],check=True,capture_output=True,timeout=10)
 def remove(path):
  path=Path(path)
  try: info=path.lstat()
  except FileNotFoundError:return
  if stat.S_ISDIR(info.st_mode):shutil.rmtree(path)
  else:path.unlink()
 for home in ('/home/ahvm','/root'):
  for name in ('.ssh','.codex','.claude','.pi','.npmrc','.git-credentials','.config/opencode','.local/share/opencode','.local/state/opencode','.local/state/ahvm-agent','.config/pi','.local/share/pi','.local/state/pi'):
   remove(Path(home)/name)
  remove(Path(home)/'.gitconfig')
 remove('/run/ahvm-project-credentials')
 remove('/var/lib/ahvm-project-environment')
 cache=Path('/var/lib/ahvm-project-environment');cache.mkdir(mode=0o700);os.chown(cache,0,0)
 # Persist only prepared installation provenance. No native run/session state
 # or previous guest identity enters a new chat's install reuse decision.
 prepared=dict(expected,run_id=metadata['build_id'].replace('-',''),boot_id='00000000-0000-0000-0000-000000000000',hook_present=metadata['install_hook_digest'] is not None,services_running=False,owner_run_id=metadata['build_id'].replace('-',''),reused=False,output_truncated=False)
 marker=cache/(expected['workspace_id']+'.json');marker.write_text(json.dumps(prepared,separators=(',',':')));os.chown(marker,0,0);marker.chmod(0o600)
 for path in Path('/tmp').glob('ahvm-project-*'):remove(path)
 for path in Path('/etc/ssh').glob('ssh_host_*'):remove(path)
 for path in (Path('/etc/machine-id'),Path('/var/lib/dbus/machine-id')):
  remove(path);path.parent.mkdir(parents=True,exist_ok=True);path.write_text('');path.chmod(0o644)
 # This trusted boot hook runs in each new private writable VM. It never adds
 # an account credential or starts a project/agent service in the template.
 directory=Path('/opt/ahvm-project-image');directory.mkdir(mode=0o755,exist_ok=True)
 assert not directory.is_symlink();os.chown(directory,0,0);directory.chmod(0o755)
 boot_script='''import os,stat,uuid,subprocess\nfrom pathlib import Path\np=Path('/etc/machine-id')\nif not p.read_text().strip():\n value=uuid.uuid4().hex+'\\n';p.write_text(value);p.chmod(0o644)\n d=Path('/var/lib/dbus/machine-id');d.write_text(value);d.chmod(0o644)\nif Path('/usr/sbin/sshd').is_file():subprocess.run(['/usr/bin/ssh-keygen','-A'],check=True)\n'''
 helper=directory/'boot.py';helper.write_text(boot_script);os.chown(helper,0,0);helper.chmod(0o644)
 manifest=directory/'provenance.json';manifest.write_text(json.dumps(metadata,separators=(',',':'))+'\n');os.chown(manifest,0,0);manifest.chmod(0o644)
 init=Path('/init.krun');assert stat.S_ISREG(init.lstat().st_mode) and init.stat().st_uid==0
 text=init.read_text();line='/usr/bin/python3 -I /opt/ahvm-project-image/boot.py\n'
 assert 'exec /usr/local/bin/ahvm-forge' in text
 if line not in text:text=text.replace('exec /usr/local/bin/ahvm-forge',line+'exec /usr/local/bin/ahvm-forge',1)
 init.write_text(text);init.chmod(0o755)
 subprocess.run(['/bin/sync'],check=True,timeout=10)
 print('project-image-source-sealed')
except BaseException:
 print('project image root proof or sanitization failed',file=sys.stderr)
 sys.exit(75)
