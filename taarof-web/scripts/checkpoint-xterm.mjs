// Real xterm checkpoint/replay differential test using fixed inert fixtures.
// No app runtime, credentials or system clipboard.
// Prerequisites: npm --prefix taarof-web ci; Node with WebSocket; Chromium.
// Run: node taarof-web/scripts/checkpoint-xterm.mjs corpus.json results.json
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const web = path.join(repo, 'taarof-web');
const output = process.argv[2] && path.resolve(process.argv[2]);
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'taarof-copy-browser-'));
fs.mkdirSync(path.join(web, '.tmp'), { recursive: true });
const fixture = fs.mkdtempSync(path.join(web, '.tmp/copy-browser-'));
const env = { ...process.env };
for (const key of Object.keys(env)) {
  if (/TOKEN|SECRET|API_KEY|INFISICAL|DISPLAY|DBUS_SESSION_BUS_ADDRESS|^TAAROF_/.test(key)) delete env[key];
}
Object.assign(env, { HOME: scratch, XDG_CONFIG_HOME: scratch, XDG_CACHE_HOME: scratch, XDG_DATA_HOME: scratch, XDG_RUNTIME_DIR: scratch });
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
let vite, chrome, socket;
const results = [];
const corpus = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
fs.writeFileSync(path.join(fixture, 'index.html'), '<link rel="icon" href="data:,"><div id="root"></div><script type="module" src="./fixture.jsx"></script>');
fs.writeFileSync(path.join(fixture, 'fixture.jsx'), `
import {Terminal} from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
window.fixture = {terminal:new Terminal({cols:40,rows:6})};
fixture.terminal.open(document.getElementById('root'));
window.capture = async (encoded, cols, rows, resize=null, tail="") => {
 const t=fixture.terminal;t.reset();t.resize(cols,rows);
 await new Promise(resolve=>t.write(Uint8Array.from(atob(encoded),c=>c.charCodeAt(0)),resolve));
 if(resize){t.resize(...resize);cols=resize[0];rows=resize[1];}
 if(tail)await new Promise(resolve=>t.write(tail,resolve));
 const b=t.buffer.active, cells=[], text=[];
 for(let y=0;y<rows;y++){
  const line=b.getLine(b.baseY+y);text.push(line.translateToString(true));
  for(let x=0;x<cols;x++){
   const c=line.getCell(x);
   cells.push([c.getChars(),c.getWidth(),c.getFgColorMode(),c.getFgColor(),c.getBgColorMode(),c.getBgColor(),c.isBold(),c.isInverse()]);
  }
 }
 return {text,cells,cursor:[b.cursorX,b.cursorY],type:b.type};
};
`);
try {
  let viteText = '';
  vite = spawn(process.execPath, [path.join(web, 'node_modules/vite/bin/vite.js'), '--host', '127.0.0.1', '--port', '0'], { cwd:web, env, stdio:['ignore','pipe','pipe'] });
  vite.stdout.on('data', b => { viteText += b; });
  vite.stderr.on('data', b => { viteText += b; });
  let origin;
  for (let n=0;n<200;n++) { origin=viteText.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0]; if(origin)break; await delay(50); }
  assert.ok(origin, 'Vite did not start');
  chrome = spawn(process.env.CHROMIUM_BIN || '/usr/bin/chromium', ['--headless','--no-sandbox','--disable-gpu','--no-first-run','--disable-dev-shm-usage','--remote-debugging-port=0',`--user-data-dir=${scratch}/chrome`,'about:blank'], {env,stdio:'ignore'});
  let port;
  for(let n=0;n<200;n++){try{port=fs.readFileSync(`${scratch}/chrome/DevToolsActivePort`,'utf8').split('\n')[0];break;}catch{}await delay(50);}
  assert.ok(port, 'Chromium did not start');
  const pages=await (await fetch(`http://127.0.0.1:${port}/json`)).json();
  socket=new WebSocket(pages.find(p=>p.type==='page').webSocketDebuggerUrl);
  await new Promise((resolve,reject)=>{socket.onopen=resolve;socket.onerror=reject;});
  let id=0; const pending=new Map();
  socket.onmessage=event=>{const m=JSON.parse(event.data);if(m.method==='Runtime.consoleAPICalled' && m.params.type==='error')console.error(m.params.args.map(x=>x.description||x.value));if(m.method==='Network.responseReceived' && m.params.response.status>=400)console.error(m.params.response.status,m.params.response.url);if(m.method==='Runtime.exceptionThrown')console.error(JSON.stringify(m.params.exceptionDetails));if(m.id){const p=pending.get(m.id);pending.delete(m.id);m.error?p.reject(new Error(m.error.message)):p.resolve(m.result);}};
  const call=(method,params={})=>new Promise((resolve,reject)=>{const n=++id;pending.set(n,{resolve,reject});socket.send(JSON.stringify({id:n,method,params}));});
  const evaluate=async expression=>{const r=await call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});if(r.exceptionDetails)throw new Error(JSON.stringify(r.exceptionDetails));return r.result?.value;};
  await call('Page.enable');
  await call('Emulation.setDeviceMetricsOverride',{width:1280,height:1400,deviceScaleFactor:1,mobile:false});
  await call('Runtime.enable');
  await call('Network.enable');
  await call('Page.navigate',{url:`${origin}/.tmp/${path.basename(fixture)}/index.html`});
  let ready=false;
  for(let n=0;n<200;n++){ready=await evaluate('Boolean(window.fixture?.terminal && document.querySelector(".xterm-screen"))');if(ready)break;await delay(50);}
  if(!ready) console.error(await evaluate('({url:location.href,body:document.body.innerText,fixture:!!window.fixture,terminal:!!window.fixture?.terminal})'),viteText);
  assert.ok(ready,'real PaneTerminal did not mount');

  for(const row of corpus){
    const direct=await evaluate(`capture(${JSON.stringify(row.source)},${row.initial_cols??row.cols},${row.initial_rows??row.rows},${JSON.stringify(row.resize??null)},${JSON.stringify(row.tail??"")})`);
    const restored=await evaluate(`capture(${JSON.stringify(row.reconstructed)},${row.cols},${row.rows})`);
    const pass=JSON.stringify(direct)===JSON.stringify(restored);
    results.push({name:row.name,split:row.split,pass,...(!pass?{direct,restored}:{})});
  }
  fs.writeFileSync(process.argv[3],JSON.stringify(results,null,2)+'\n');
  const failures=results.filter(r=>!r.pass);
  console.log(JSON.stringify({total:results.length,failed:failures.length,names:[...new Set(failures.map(r=>r.name))]}));
  assert.equal(failures.length,0,'xterm checkpoint differential failures');
} finally {
  socket?.close();
  const stop=async child=>{if(child&&child.exitCode===null){const exited=new Promise(resolve=>child.once('exit',resolve));child.kill('SIGTERM');await Promise.race([exited,delay(2000)]);if(child.exitCode===null)child.kill('SIGKILL');}};
  await stop(chrome); await stop(vite);
  fs.rmSync(fixture,{recursive:true,force:true});fs.rmSync(scratch,{recursive:true,force:true});
}
