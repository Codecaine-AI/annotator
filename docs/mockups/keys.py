import re,pathlib
CSS=""".app{--a-red:#f04438;--a-accent:#3b5cf6;font-family:-apple-system,BlinkMacSystemFont,"SF Pro Text",system-ui,sans-serif;font-size:13px}
html,body{margin:0;padding:0;background:none}.scroll{overflow-x:auto}
.desk{border-radius:10px;background:linear-gradient(135deg,#d7c9f1,#a9c6ec 45%,#f2c7a7);min-width:520px;display:grid;gap:14px;padding:18px 12px}
.lane{display:grid;grid-template-columns:150px 1fr;align-items:center;gap:12px}
.lbl{font-size:12px;color:#1f1f1f;line-height:1.3}.lbl b{display:block;font-weight:600}
.pill{justify-self:start;display:inline-flex;align-items:center;height:40px;padding:0 4px;border-radius:20px;background:rgba(20,20,22,.96);box-shadow:0 4px 12px rgba(0,0,0,.16)}
.chip{display:inline-flex;align-items:center;height:32px;padding:0 12px;border-radius:16px;gap:8px;white-space:nowrap}
.dot{width:8px;height:8px;border-radius:50%;background:var(--a-red);box-shadow:0 0 0 3px rgba(240,68,56,.25)}
.clock{color:#f5f5f7;font-variant-numeric:tabular-nums}.div{width:1px;height:16px;background:rgba(255,255,255,.11);margin:0 6px}
.cancel{color:rgba(255,255,255,.74)}.finish{background:var(--a-accent);color:#f2f2f5}
.k{font-size:11px;padding:1px 5px;border-radius:4px;background:rgba(255,255,255,.1);color:rgba(255,255,255,.6);letter-spacing:.5px}
.finish .k{background:rgba(255,255,255,.18);color:rgba(255,255,255,.85)}
.new{outline:1.5px dashed #6ea0ff;outline-offset:2px}
.wave{display:inline-flex;align-items:center;gap:2px;width:84px;height:32px;padding:0 6px;-webkit-mask-image:linear-gradient(90deg,transparent,#000 12px);mask-image:linear-gradient(90deg,transparent,#000 12px)}
.wave i{width:2px;border-radius:1px;background:#f5f5f7;flex:none}"""
hs=[3,4,6,9,14,18,12,8,5,7,11,16,20,15,10,6,4,8,13,17,9,5]
wave='<span class="wave">'+''.join(f'<i style="height:{h}px;opacity:{0.36+0.6*h/20:.2f}"></i>' for h in hs)+'</span>'
def pill(c,f): return f'<div class="pill"><span class="chip"><span class="dot"></span><span class="clock">0:12</span></span>{wave}<span class="div"></span><span class="chip cancel">Cancel <span class="k new">{c}</span></span><span class="chip finish">Finish <span class="k new">{f}</span></span></div>'
body=f'<div class="scroll"><div class="app desk"><div class="lane"><div class="lbl"><b>Default keys</b>Esc cancels, Record finishes</div>{pill("esc","⌥⇧S")}</div><div class="lane"><div class="lbl"><b>After a change</b>The capsule follows Settings</div>{pill("F1","⌃⌥R")}</div></div></div>'
s=f'<!doctype html><html><head><meta charset="utf-8"><title>Capsule Keys</title><style>{re.sub(chr(10),"",CSS)}</style></head><body>{body}</body></html>'
pathlib.Path(__file__).parent.joinpath('out/25-capsule-keys.html').write_text(s); print(len(s))
