"""Reproduce CLI audit observations in isolated indexes/socket; no user daemon."""
import hashlib,json,os,pathlib,subprocess,tempfile,time
P=pathlib.Path
binary=P('target/release/fxi').resolve()
base=P(tempfile.mkdtemp(prefix='fxi-cli-audit-'))
root=base/'repo';root.mkdir();(root/'sub').mkdir()
subprocess.run(['git','init','-q',str(root)],check=True)
(root/'a.rs').write_text('alpha beta\nALPHA\nplain\nalpha alpha\nfoo/bar\n')
(root/'b.py').write_text('alpha\nbeta\n')
(root/'upper.rs').write_text('ALPHA\n')
(root/'sub'/'c.rs').write_text('alpha beta\n')
(root/'odd\nname.rs').write_text('alpha\n')
env={**os.environ,'FXI_INDEXES':str(base/'indexes'),'FXI_SOCKET':str(base/'socket'),'FXI_STALE_WARN_SECS':'0'}
rows=[]
def run(label,args,cwd=root,input=None):
    try:
        r=subprocess.run([str(binary),*args],cwd=cwd,env=env,input=input,capture_output=True,timeout=15)
        row=dict(label=label,args=args,code=r.returncode,stdout=r.stdout.decode(errors='replace'),stderr=r.stderr.decode(errors='replace'))
    except subprocess.TimeoutExpired:
        row=dict(label=label,args=args,timeout=True)
    rows.append(row);return row
run('build',['index','--force',str(root)])
for label,args in [
 ('help',['--help']),('version',['--version']),('no_args_pipe',[]),('empty',['']),
 ('absent',['absentUniqueZZZ']),('invalid_regex',['re:/[/']),('unsupported_invert',['-v','alpha']),
 ('subtree',['-l','alpha','-p',str(root/'sub')]),('single_file',['-l','alpha','-p',str(root/'a.rs')]),
 ('positional_path',['alpha',str(root/'sub')]),('reserved_word',['stats']),('reserved_escape',['--','stats']),
 ('single_e',['-l','-e','alpha']),('multiple_e',['-l','-e','alpha','-e','unmatchedZZZ']),
 ('word_case',['-l','-w','alpha']),('word_query',['-l','-w','alpha beta']),
 ('regex_e',['-l','-e','re:/alpha/','-e','re:/beta/']),
 ('slash_e',['-l','-e','foo/bar','-e','absentZZZ']),
 ('slash_word',['-l','-w','foo/bar']),
 ('context',['-C','1','alpha']),('boolean_lines',['alpha beta']),('boolean_counts',['-c','alpha beta']),
 ('counts',['-c','alpha']),('limit_counts',['-c','-m','1','alpha']),
 ('negative',['--','-absentZZZ']),('hyphen',['-absentZZZ']),
 ('bad_filter',['alpha size:bogus']),('bad_glob',['alpha path:[']),
 ('filter_or',['-l','ext:rs | ext:py']),('sort',['alpha sort:nope']),
 ('limit_top',['alpha top:1']),('newline_paths',['-l','alpha']),
 ('reload_without_daemon',['daemon','reload']),('status_without_daemon',['daemon','status']),
 ('bad_path',['alpha','-p',str(root/'missing')]),
]:run(label,args,input=b'')
# Use a dedicated foreground daemon, then verify its control-plane behavior.
log=(base/'daemon.log').open('wb');server=subprocess.Popen([str(binary),'daemon','foreground'],cwd=root,env=env,stdout=log,stderr=log)
try:
 for _ in range(100):
  if (base/'socket').exists():break
  time.sleep(.02)
 run('daemon_query',['-l','alpha'])
 run('daemon_start_upgrade_watch',['daemon','start','--watch'])
 run('daemon_index_after_start_watch',['index'])
 run('daemon_status',['daemon','status'])
 # Explicit force rebuild should be visible to a daemon even without watch.
 (root/'added.rs').write_text('freshUniqueAfterForce\n')
 run('force_with_live_daemon',['index','--force'])
 run('query_after_force',['-l','freshUniqueAfterForce'])
 run('reload_after_force',['daemon','reload'])
 run('query_after_reload',['-l','freshUniqueAfterForce'])
 run('remove_loaded',['remove',str(root)])
 run('query_after_remove',['-l','alpha'])
 run('list_after_remove',['list'])
finally:
 run('daemon_stop',['daemon','stop'])
 if server.poll() is None:server.terminate()
 server.wait(timeout=10);log.close()
# Rebuild index to probe downstream pipe behavior using enough output.
(root/'big.rs').write_text('alpha\n'*20000)
run('build_pipe_fixture',['index','--force'])
p=subprocess.Popen([str(binary),'alpha'],cwd=root,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
p.stdout.readline();p.stdout.close();err=p.stderr.read();rc=p.wait(timeout=15)
rows.append(dict(label='broken_pipe',code=rc,stderr=err.decode(errors='replace')))
run('stats_before_delete',['stats'])
(root/'b.py').unlink()
run('index_after_delete',['index'])
run('stats_after_delete',['stats'])
unindexed=base/'unindexed';unindexed.mkdir();(unindexed/'x.txt').write_text('needle\n')
run('unindexed_search',['needle','-p',str(unindexed)])
out=dict(binary=str(binary),sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),base=str(base),rows=rows)
P('docs/audit-2026-09-18/cli-observations.json').write_text(json.dumps(out,indent=2)+'\n')
for r in rows:print(r['label'],r.get('code'),repr(r.get('stdout','')[:170]),repr(r.get('stderr','')[:170]))
