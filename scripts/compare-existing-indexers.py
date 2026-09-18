"""Recheck pinned stock CLIs against existing immutable snapshot indexes."""
import argparse, hashlib, json, os, pathlib, random, shutil, statistics, subprocess, tempfile, time
p = argparse.ArgumentParser()
p.add_argument('--snapshot', type=pathlib.Path, required=True)
p.add_argument('--fxi', type=pathlib.Path, required=True)
p.add_argument('--tgrep', type=pathlib.Path, required=True)
p.add_argument('--fxi-indexes', type=pathlib.Path, required=True, help='Current lean compressed FXI index directory')
p.add_argument('--patterns', nargs='+')
p.add_argument('--output', type=pathlib.Path, required=True)
p.add_argument('--repetitions', type=int, default=11)
a = p.parse_args()
assert a.repetitions > 0
old = json.loads(a.snapshot.read_text()); root = pathlib.Path(old['corpus']); base = pathlib.Path(old['base'])
runtime = pathlib.Path(tempfile.mkdtemp(prefix='fxi-post-audit-compare-'))
binaries = {'fxi': str(a.fxi.resolve()), 'fxi-packed': str(a.fxi.resolve()), 'fxi-lean': str(a.fxi.resolve()), 'tgrep': str(a.tgrep.resolve()),
            'csearch': old['binaries']['csearch']['path'], 'zoekt': old['binaries']['zoekt']['path'], 'ripgrep': shutil.which('rg')}
env = dict(os.environ, FXI_INDEXES=str(base/'fxi'), FXI_SOCKET=str(runtime/'unused.sock'),
           XDG_RUNTIME_DIR=str(runtime), CSEARCHINDEX=str(base/'csearch.idx'))
for key in ['FXI_NEGATIVE_ROUTING', 'FXI_SEARCH_PARALLELISM', 'RAYON_NUM_THREADS', 'RIPGREP_CONFIG_PATH']:
    env.pop(key, None)
def command(tool, pattern):
    binary = binaries[tool]
    if tool.startswith('fxi'): return [binary, '-l', '--color=never', 're:/'+pattern+'/', '-p', str(root)]
    if tool == 'zoekt': return [binary, '-index_dir', str(base/'zoekt'), '-l', 'case:yes type:file content:'+json.dumps(pattern)]
    if tool == 'csearch': return [binary, '-l', pattern]
    if tool == 'tgrep': return [binary, '-s', '-l', '--color=never', pattern, str(root)]
    return [binary, '-l', '--color=never', pattern, '.']
def run(tool, pattern):
    child_env = dict(env, FXI_SOURCE_PACK='1' if tool=='fxi-packed' else '0')
    if tool in ('fxi-lean', 'fxi-packed'): child_env['FXI_INDEXES'] = str(a.fxi_indexes.resolve())
    started = time.perf_counter_ns()
    r = subprocess.run(command(tool,pattern), cwd=root, env=child_env, capture_output=True, timeout=120)
    ms = (time.perf_counter_ns()-started)/1e6
    assert r.returncode in ((0,) if tool.startswith('fxi') or tool=='zoekt' else (0,1)), (tool,r.stderr.decode())
    assert b'Daemon search failed' not in r.stderr, (tool,r.stderr.decode())
    if tool=='csearch': assert not r.stderr, r.stderr
    paths=[]
    for line in r.stdout.decode().splitlines():
        path=pathlib.Path(line); paths.append(str(path.relative_to(root)) if path.is_absolute() else line.removeprefix('./'))
    assert len(paths)==len(set(paths)), ('duplicate',tool)
    return ms,set(paths)
result={'source_snapshot': str(a.snapshot), 'corpus': str(root), 'manifest_sha256': old['manifest_sha256'],
        'mode':'direct CLI; warm filesystem; case-sensitive regex; complete files-only output',
        'query_local': env.get('FXI_QUERY_LOCAL', '0'),
        'note':'fxi: full index, packs off; fxi-lean: lean index, packs off; fxi-packed: lean index, compressed packs on. Existing indexes; no new build/size ranking.',
        'fxi_indexes': {'full': str(base/'fxi'), 'lean': str(a.fxi_indexes.resolve())},
        'harness_sha256':hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),
        'binaries':{k:{'path':v,'sha256':hashlib.sha256(pathlib.Path(v).read_bytes()).hexdigest()} for k,v in binaries.items()},'coverage':{},'rows':[]}
if not (root/'.tgrep').exists():
    r=subprocess.run([binaries['tgrep'],'index',str(root)],env=env,capture_output=True,timeout=180)
    assert r.returncode==0,r.stderr
    result['tgrep_index_log']=r.stderr.decode()
def manifest():
    names = sorted(name for name in subprocess.check_output(['rg', '--files', '-0'], cwd=root).decode().split('\0') if name)
    digest = hashlib.sha256()
    for name in names:
        digest.update(json.dumps([name, hashlib.sha256((root/name).read_bytes()).hexdigest()], separators=(',', ':')).encode() + b'\n')
    return digest.hexdigest()
initial_manifest = manifest()
assert initial_manifest == old['manifest_sha256'], 'Corpus differs from source snapshot'
expected=run('ripgrep','^')[1]
assert len(expected)==old['files'],(len(expected),old['files'])
for tool in binaries:
    actual=run(tool,'^')[1]
    result['coverage'][tool]={'files':len(actual),'missing':sorted(expected-actual),'extra':sorted(actual-expected)}
    assert actual==expected,(tool,result['coverage'][tool])
for pattern in (a.patterns or [row['pattern'] for row in old['rows']]):
    expected=run('ripgrep',pattern)[1]; samples={tool:[] for tool in binaries}
    for repetition in range(-1,a.repetitions):
        order=list(binaries);random.Random(1729+repetition).shuffle(order)
        for tool in order:
            elapsed,actual=run(tool,pattern)
            assert actual==expected,(tool,pattern,sorted(expected-actual)[:5],sorted(actual-expected)[:5])
            if repetition>=0:samples[tool].append(elapsed)
    row={'pattern':pattern,'files':len(expected),'tools':{tool:{'median_ms':statistics.median(values),'samples_ms':values} for tool,values in samples.items()}}
    result['rows'].append(row);a.output.write_text(json.dumps(result,indent=2)+'\n')
    print(pattern,{tool:round(v['median_ms'],2) for tool,v in row['tools'].items()},flush=True)

assert manifest() == initial_manifest, 'Corpus changed during measurements'
result['manifest_verified_before_and_after'] = True
a.output.write_text(json.dumps(result, indent=2)+'\n')
