#!/usr/bin/env python3
"""Real Claude TUI protocol test against a local, deterministic API fixture."""
import os,sys,json,time,shlex,subprocess,threading,tempfile,uuid,shutil
from pathlib import Path
from http.server import BaseHTTPRequestHandler,HTTPServer
temp=tempfile.TemporaryDirectory(prefix='claude-status-probe-');root=Path(temp.name)
config=root/'config';config.mkdir(exist_ok=True)
(config/'.claude.json').write_text(json.dumps({'hasCompletedOnboarding':True,'theme':'dark','lastOnboardingVersion':'2.1.295','customApiKeyResponses':{'approved':['test-only'],'rejected':[]},'projects':{str(root.resolve()):{'hasTrustDialogAccepted':True}}}))
class API(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(b'{}')
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}')
  if 'count_tokens' in self.path:
   self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(b'{"input_tokens":100}');return
  has_result=any(isinstance(m.get('content'),list) and any(c.get('type')=='tool_result' for c in m['content']) for m in body.get('messages',[]))
  content={'type':'text','text':'OSC7501_TEST_COMPLETE'} if has_result else {'type':'tool_use','id':'tool_osc_question','name':'AskUserQuestion','input':{'questions':[{'question':'Which test option?','header':'Probe','options':[{'label':'Alpha','description':'Choose Alpha'},{'label':'Beta','description':'Choose Beta'}],'multiSelect':False}]}}
  response={'id':'msg_osc_probe','type':'message','role':'assistant','model':'claude-sonnet-4-6','content':[content],'stop_reason':'end_turn' if has_result else 'tool_use','stop_sequence':None,'usage':{'input_tokens':100,'output_tokens':20}}
  (root/'requests.txt').open('a').write(('final' if has_result else 'question')+'\n')
  self.send_response(200)
  if not body.get('stream'):
   self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps(response).encode());return
  self.send_header('Content-Type','text/event-stream');self.end_headers()
  start=dict(response,content=[],stop_reason=None,usage={'input_tokens':100,'output_tokens':0})
  events=[('message_start',{'message':start}),('content_block_start',{'index':0,'content_block':dict(content,**({'text':''} if has_result else {'input':{}}))}),('content_block_delta',{'index':0,'delta':{'type':'text_delta','text':content['text']} if has_result else {'type':'input_json_delta','partial_json':json.dumps(content['input'])}}),('content_block_stop',{'index':0}),('message_delta',{'delta':{'stop_reason':response['stop_reason'],'stop_sequence':None},'usage':{'output_tokens':20}}),('message_stop',{})]
  for name,data in events:
   self.wfile.write(f'event: {name}\ndata: {json.dumps(dict(type=name,**data))}\n\n'.encode());self.wfile.flush();time.sleep(.05)
server=HTTPServer(('127.0.0.1',0),API);threading.Thread(target=server.serve_forever,daemon=True).start()
summon=str(Path(sys.argv[1]).resolve())
claude=str(Path(sys.argv[2]).resolve())
tmux=shutil.which('tmux')
socket='claude-status-'+uuid.uuid4().hex
inner=shlex.join(['env','-i','PATH='+os.environ['PATH'],'HOME='+os.environ['HOME'],'TERM=xterm-256color','CLAUDE_CONFIG_DIR='+str(config),'ANTHROPIC_API_KEY=test-only','ANTHROPIC_BASE_URL=http://127.0.0.1:'+str(server.server_port),'DISABLE_AUTOUPDATER=1',claude,'--debug-file',str(root/'debug.txt'),'--setting-sources','','--settings','{"disableAllHooks":true}','--model','claude-sonnet-4-6','--tools','AskUserQuestion','--strict-mcp-config','--mcp-config','{"mcpServers":{}}'])
cmd=shlex.join([summon,'program-status-run',str(root/'status.json'),'live-probe',inner])
subprocess.run([tmux,'-L',socket,'-f','/dev/null','new-session','-d','-s','probe','-c',str(root),cmd],check=True)
def tmux_call(*args):
 return subprocess.run([tmux,'-L',socket,*args],capture_output=True,text=True,check=True).stdout
try:
 end=time.monotonic()+60
 states=[];sent=False;answered=False
 while time.monotonic()<end:
  screen=tmux_call('capture-pane','-p','-t','=probe:')
  try:
   snapshot=json.loads((root/'status.json').read_text())
   report=snapshot.get('report') or {};state=report.get('state')
   if state and (not states or states[-1]!=state):states.append(state)
   if state=='idle' and not sent:
    tmux_call('send-keys','-t','=probe:','-l','Ask me which option to choose using AskUserQuestion.');tmux_call('send-keys','-t','=probe:','Enter');sent=True
   if state=='blocked' and report.get('kind')=='question' and not answered:
    tmux_call('send-keys','-t','=probe:','Enter');answered=True
   if state=='done' and 'OSC7501_TEST_COMPLETE' in screen:break
  except (FileNotFoundError,json.JSONDecodeError):pass
  time.sleep(.05)
 else:raise AssertionError({'states':states,'screen':screen})
 debug=(root/'debug.txt').read_text()
 assert 'programStatus=yes' in debug
 assert all(state in states for state in ['idle','working','blocked','done']),states
 print(json.dumps({'claude':subprocess.check_output([claude,'--version'],text=True).strip(),'states':states,'capability':'programStatus=yes','inference':'local mock API','completion':'OSC7501_TEST_COMPLETE'}))
finally:
 subprocess.run([tmux,'-L',socket,'kill-server'],capture_output=True)
 server.shutdown()
 time.sleep(2.2)
 temp.cleanup()
