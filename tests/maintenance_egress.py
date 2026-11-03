#!/usr/bin/env python3
"""Run only in an isolated network namespace with loopback enabled."""
import os,sys,json,time,threading,subprocess,tempfile,queue,socket,socketserver,http.server,urllib.parse,select,struct,base64,datetime,collections
from pathlib import Path
ROOT=Path(__file__).resolve().parent
EVENTS=[];SOCKS_PORTS=set()
class HTTP(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def do_GET(self):self.respond()
 def do_POST(self):self.respond()
 def respond(self):
  size=int(self.headers.get('Content-Length','0'));body=self.rfile.read(size) if size else b''
  path=urllib.parse.urlsplit(self.path).path
  route='socks5' if self.client_address[1] in SOCKS_PORTS else self.server.route
  EVENTS.append({'route':route,'method':self.command,'path':path})
  if route=='http' and self.headers.get('Proxy-Authorization')!='Basic '+base64.b64encode(b'fixture:only-test').decode():
   self.send_response(407);self.send_header('Content-Length','0');self.end_headers();return
  if path=='/api/web/points/v1/balance':data={'available_points':12345,'daily_points':300}
  elif path=='/api/web/points/v2/bills':
   day=datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).isoformat()
   data={'items':[{'biz_type':'daily_grant','points':300,'created_at':day}],'has_more':False}
  elif path.endswith('/login/points/grant'):data={'granted':True,'popup':{'points':3000}}
  elif path=='/api/v2/points/records':data={'balance':500,'dailyBalance':5000}
  elif path=='/api/v1/models':data=[{'id':'fixture-model','protocol':'openai'}]
  elif path=='/api/v1/onboarding/tasks':data={'tasks':{}}
  elif path=='/api/v1/onboarding/tasks/complete':data={'alreadyCompleted':False}
  else:data={}
  result={'code':'000000' if path.startswith('/api/v') else 0,'data':data}
  raw=json.dumps(result).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(raw)));self.end_headers();self.wfile.write(raw)
class Server(http.server.ThreadingHTTPServer):daemon_threads=True
class Socks(socketserver.BaseRequestHandler):
 def read(self,n):
  b=b''
  while len(b)<n:
   c=self.request.recv(n-len(b))
   if not c:raise EOFError()
   b+=c
  return b
 def handle(self):
  try:
   ver,n=self.read(2);methods=self.read(n);self.request.sendall(b'\x05\x02')
   ver,n=self.read(2);user=self.read(n);n=self.read(1)[0];password=self.read(n)
   if (user,password)!=(b'fixture',b'only-test'):self.request.sendall(b'\x01\x01');return
   self.request.sendall(b'\x01\x00');ver,cmd,res,typ=self.read(4)
   if typ==1:host=socket.inet_ntoa(self.read(4))
   elif typ==3:host=self.read(self.read(1)[0]).decode()
   else:raise ValueError('unsupported test address')
   port=struct.unpack('!H',self.read(2))[0]
   # Fixture never connects anywhere except the local mock upstream.
   assert host=='127.0.0.1' and port==UPSTREAM.server_port
   with socket.create_connection((host,port),timeout=5) as peer:
    SOCKS_PORTS.add(peer.getsockname()[1]);self.request.sendall(b'\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00')
    while True:
     ready,_,_=select.select([self.request,peer],[],[],8)
     if not ready:return
     for src in ready:
      data=src.recv(65536)
      if not data:return
      (peer if src is self.request else self.request).sendall(data)
  except (EOFError,ConnectionError,OSError):pass
class SocksServer(socketserver.ThreadingTCPServer):allow_reuse_address=True;daemon_threads=True
UPSTREAM=Server(('127.0.0.1',0),HTTP);UPSTREAM.route='direct'
PROXY=Server(('127.0.0.1',0),HTTP);PROXY.route='http'
SOCKS=SocksServer(('127.0.0.1',0),Socks)
for s in [UPSTREAM,PROXY,SOCKS]:threading.Thread(target=s.serve_forever,daemon=True).start()

def session(binary,provider,mode,operation):
 EVENTS.clear();SOCKS_PORTS.clear();callbacks=[]
 port=SOCKS.server_address[1] if mode=='socks5' else PROXY.server_port
 proxy={'protocol':'socks5' if mode=='socks5' else 'http','host':'127.0.0.1','port':port,'username':'fixture','password':'only-test'}
 if mode=='dead':proxy['port']=1
 if mode=='invalid':proxy['port']=0
 env=os.environ.copy();env['RACCOON_MAIN_SITE_URL']=f'http://127.0.0.1:{UPSTREAM.server_port}';env['LOOMY_POINTS_BASE_URL']=env['RACCOON_MAIN_SITE_URL']
 # A gateway proxy is intentionally set: explicit direct must override it for Loomy.
 with tempfile.TemporaryDirectory() as d, tempfile.TemporaryFile(mode='w+t') as err:
  p=subprocess.Popen([binary],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=err,text=True,env=env,bufsize=1)
  q=queue.Queue()
  def reader():
   for line in p.stdout:q.put(line)
  threading.Thread(target=reader,daemon=True).start()
  def send(x):p.stdin.write(json.dumps(x)+'\n');p.stdin.flush()
  def call(i,method,params):
   send({'id':i,'method':method,'params':params})
   while True:
    frame=json.loads(q.get(timeout=40))
    if frame.get('method'):
     m=frame['method'];callbacks.append(m)
     if m=='store.get':result={'id':'fixture-account','provider':provider,'secret':{'accessToken':'fixture-token','session':'fixture-session','userId':'fixture-user'},'public':{},'proxy':{'state':'failed','message':'account proxy must not be used'}}
     elif m=='store.list':result=[]
     elif m=='store.ledger_get':result=None
     elif m=='store.ledger_put':result=True
     else:raise AssertionError('unexpected callback '+m)
     send({'id':frame['id'],'result':result})
    elif frame.get('id')==i and ('result' in frame or 'error' in frame):return frame
  try:
   result=call(1,'init',{'env':{'data_dir':d,'host_version':'3.4.0','target':'x86_64-unknown-linux-gnu','proxy':f'http://fixture:only-test@127.0.0.1:{PROXY.server_port}','debug_traffic':False}})
   assert 'error' not in result,result
   caps=call(2,'maintenance.capabilities',{'provider':provider})
   assert caps['result']['version']==1,caps
   assert operation in caps['result']['operations'],caps
   model_params={'provider':provider,'account':'fixture-account','proxy':None,
     'body':{'model':'fixture-model','messages':[{'role':'user','content':'fixture'}]},
     'headers':{},'stream':False,'wire_model':'fixture-model','reasoning':''}
   model_before=None
   if mode=='http' and operation=='usage.query':
    model_before=call(20,'chat',model_params)
    assert 'error' not in model_before,model_before
    EVENTS.clear();SOCKS_PORTS.clear()
   params={'provider':provider,'account':'fixture-account','manual':True,'refresh':True,'claim':''}
   if mode!='legacy':params['maintenanceEgress']={'version':1,'proxy':None if mode=='direct' else proxy}
   result=call(3,operation,params)
   routes=sorted(set(e['route'] for e in EVENTS))
   if provider=='raccoon' and operation=='claim.status' and mode!='invalid':
    assert not EVENTS,(mode,EVENTS)  # Local ledger status does not issue an upstream request.
    assert 'error' not in result,result
   elif mode in ['dead','invalid']:
    assert not EVENTS,(mode,EVENTS)
    assert 'error' in result or result.get('result',{}).get('failed',0)>0 or result.get('result',{}).get('outcome')=='failed',result
   else:
    expected='http' if mode=='legacy' and provider=='loomy' else ('direct' if mode in ['legacy','direct'] else mode)
    assert routes==[expected],(provider,mode,operation,routes,result)
    assert 'error' not in result,result
   # An injected maintenance override must never affect model/chat dispatch.
   guarded=call(4,'chat',{'provider':provider,'maintenanceEgress':{'version':1,'proxy':proxy}})
   assert 'error' in guarded and guarded['error'].get('code')=='unsupported',guarded
   if model_before is not None:
    model_after=call(21,'chat',model_params)
    def stable_plan(frame):
     plan=dict(frame['result']);plan['headers']=[(k,v) for k,v in plan.get('headers',[]) if k.lower() not in ['traceparent','chatid','msgid','turnid']];return plan
    assert stable_plan(model_before)==stable_plan(model_after),(model_before,model_after)
   return {'provider':provider,'operation':operation,'mode':mode,'routes':routes,'passed':True,'model_plan_unchanged':model_before is not None}
  finally:
   p.stdin.close()
   try:p.wait(timeout=5)
   except subprocess.TimeoutExpired:p.kill();p.wait()
   assert p.returncode==0,p.returncode

def main():
 rows=[]
 try:
  for provider in ['raccoon','loomy']:
   for operation in ['usage.query','checkin.claim','claim','claim.status']:
    for mode in ['legacy','direct','http','socks5','dead','invalid']:
     row=session(sys.argv[1],provider,mode,operation);rows.append(row);print(json.dumps(row),flush=True)
  print(json.dumps({'passed':len(rows),'real_upstream':False,'accounts':'synthetic only'}))
 finally:
  for s in [UPSTREAM,PROXY,SOCKS]:s.shutdown();s.server_close()
if __name__=='__main__':main()
