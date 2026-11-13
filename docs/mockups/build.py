"""Build the Annotator plan mockups: one self-contained HTML file per screen.
App colors come from the Scrawl source (model.rs, pill.metal, Settings.swift, ui.rs).
Frame and caption colors come from design-system tokens in root.css."""
import pathlib, sys
here = pathlib.Path(__file__).parent  # docs/mockups
out = here / "out"; out.mkdir(exist_ok=True)
root = (here / "root.css").read_text()

BASE = """
html,body{margin:0;padding:0;background:none}
body{font-family:var(--ds-font-family-sans);color:var(--ds-color-text-body)}
.scroll{overflow-x:auto}
.cap{font-size:var(--ds-font-size-ui-sm);color:var(--ds-color-text-muted);margin:var(--ds-space-2) 0 0;line-height:1.45}
.cap b{color:var(--ds-color-text-ink);font-weight:var(--ds-font-weight-semibold)}
.tag{display:inline-block;font-size:var(--ds-font-size-ui-2xs);font-weight:var(--ds-font-weight-medium);padding:1px 6px;border-radius:var(--ds-radius-pill);background:var(--ds-color-blue-soft);color:var(--ds-color-blue-text);margin-right:6px}
.tag.now{background:var(--ds-color-surface-sunken);color:var(--ds-color-text-muted)}
/* App colors, copied from the Scrawl source. The app UI is dark in both themes. */
.app{--a-win:#1e1e1e;--a-side:#262626;--a-group:rgba(255,255,255,.045);--a-line:rgba(255,255,255,.08);
 --a-text:#f2f2f2;--a-dim:rgba(255,255,255,.55);--a-faint:rgba(255,255,255,.38);--a-accent:#3b5cf6;--a-switch:#0a84ff;
 --a-red:#f04438;--a-green:#69d89f;font-family:-apple-system,BlinkMacSystemFont,"SF Pro Text",system-ui,sans-serif;
 color:var(--a-text);font-size:13px}
.win{background:var(--a-win);border-radius:12px;overflow:hidden;box-shadow:0 0 0 1px rgba(255,255,255,.1),0 8px 24px rgba(0,0,0,.25)}
.lights{display:flex;gap:8px}.lights i{width:12px;height:12px;border-radius:50%;background:#ff5f57}.lights i+i{background:#febc2e}.lights i+i+i{background:#28c840}
.kbd{white-space:nowrap;flex:none;display:inline-flex;align-items:center;gap:6px;height:24px;padding:0 10px;border-radius:6px;background:rgba(255,255,255,.08);box-shadow:inset 0 0 0 1px rgba(255,255,255,.12);font-size:13px;letter-spacing:1px;color:var(--a-text)}
.kbd .x{letter-spacing:0;color:var(--a-faint);font-size:11px}
.pop{white-space:nowrap;flex:none;display:inline-flex;align-items:center;gap:8px;height:24px;padding:0 8px 0 10px;border-radius:6px;background:rgba(255,255,255,.1);font-size:13px}
.pop:after{content:"⌃";transform:rotate(180deg);font-size:10px;color:var(--a-dim)}
.sw{width:32px;height:18px;border-radius:9px;background:rgba(255,255,255,.18);position:relative;flex:none}
.sw:after{content:"";position:absolute;top:2px;left:2px;width:14px;height:14px;border-radius:50%;background:#fff}
.sw.on{background:var(--a-switch)}.sw.on:after{left:16px}
.new{outline:1.5px dashed #6ea0ff;outline-offset:2px;border-radius:6px}
"""

SETTINGS_CSS = """
.set{display:grid;grid-template-columns:180px 1fr;min-width:560px;max-width:640px}
.side{background:var(--a-side);padding:16px 10px}
.side .lights{margin:0 6px 22px}
.side h6{margin:12px 8px 4px;font-size:11px;font-weight:600;color:var(--a-faint)}
.item{display:flex;align-items:center;gap:8px;height:30px;padding:0 8px;border-radius:7px;font-size:13px}
.item.sel{background:rgba(255,255,255,.1)}
.ic{width:20px;height:20px;border-radius:5px;display:grid;place-items:center;font-size:11px;color:#fff}
.main{padding:18px 20px 20px}
.main h2{margin:0 0 14px;font-size:15px;font-weight:600}
.grp{background:var(--a-group);border-radius:12px;margin-bottom:14px}
.row{display:flex;align-items:center;justify-content:space-between;gap:12px;min-height:42px;padding:6px 14px;border-top:1px solid var(--a-line)}
.row:first-child{border-top:0}
.row small{display:block;color:var(--a-dim);font-size:11.5px;margin-top:2px;line-height:1.35}
.hd{font-size:11px;font-weight:600;color:var(--a-faint);margin:0 4px 6px}
.err{color:#ff8a80;font-size:11.5px;margin-top:3px}
"""

def sidebar(sel):
    items = [("General","#8e8e93","⚙",None),("hdr","Settings",None,None),("Recording","#e5484d","●",None),
             ("Shortcuts","#7c5cf6","⌘","new"),("Storage","#3b82f6","▤",None),("Permissions","#30a46c","✋",None),
             ("Agents","#3a3a3c","›_",None),("hdr","Annotator",None,None),("About","#8e8e93","i",None)]
    h = ['<div class="side"><div class="lights"><i></i><i></i><i></i></div>']
    for name,col,ic,flag in items:
        if name=="hdr": h.append(f'<h6>{col}</h6>'); continue
        cls = "item"+(" sel" if name==sel else "")+(" new" if flag else "")
        h.append(f'<div class="{cls}"><span class="ic" style="background:{col}">{ic}</span>{name}</div>')
    h.append('</div>'); return "".join(h)

def page(title, css, body):
    return f'<!doctype html><html><head><meta charset="utf-8"><title>{title}</title><style>\n{root}\n{BASE}\n{css}\n</style></head><body>\n{body}\n</body></html>\n'

# ---------- 1. Capsule ----------
import random
random.seed(7)
def wave(kind="talk"):
    bars=[]
    if kind=="talk":
        hs=[3,4,6,9,14,18,12,8,5,7,11,16,20,15,10,6,4,8,13,17,9,5]
        for i,h in enumerate(hs):
            a=0.36+0.6*(h/20); 
            if i<3: a*= (i+1)/4
            bars.append(f'<i style="height:{h}px;opacity:{a:.2f}"></i>')
    elif kind=="quiet":
        bars=['<i style="height:2px;opacity:.36"></i>']*22
    return f'<span class="wave {kind}">'+"".join(bars)+'</span>' if kind!="dead" else '<span class="wave dead"><b></b></span>'

CAP_CSS = """
.desk{border-radius:10px;overflow:hidden;background:linear-gradient(135deg,#d7c9f1,#a9c6ec 45%,#f2c7a7);min-width:520px}
.menubar{height:22px;background:rgba(255,255,255,.55);display:flex;align-items:center;justify-content:space-between;padding:0 12px;font-size:12px;color:#1f1f1f}
.menubar span{display:flex;gap:14px;align-items:center}
.stage{display:grid;justify-items:center;gap:14px;padding:16px 12px 22px}
.lane{display:grid;grid-template-columns:150px 1fr;align-items:center;gap:12px;width:100%;max-width:680px}
.lane .lbl{font-size:12px;color:#1f1f1f;line-height:1.3}
.lane .lbl b{display:block;font-weight:600}
.pill{justify-self:start;display:inline-flex;align-items:center;height:40px;padding:0 4px;border-radius:20px;background:rgba(20,20,22,.96);box-shadow:0 4px 12px rgba(0,0,0,.16)}
.chip{display:inline-flex;align-items:center;height:32px;padding:0 12px;border-radius:16px;gap:8px;white-space:nowrap}
.dot{width:8px;height:8px;border-radius:50%;background:var(--a-red);box-shadow:0 0 0 3px rgba(240,68,56,.25)}
.clock{color:#f5f5f7;font-variant-numeric:tabular-nums}
.div{width:1px;height:16px;background:rgba(255,255,255,.11);margin:0 6px}
.cancel{color:rgba(255,255,255,.74)}
.finish{background:var(--a-accent);color:#f2f2f5}
.wave{display:inline-flex;align-items:center;gap:2px;width:84px;height:32px;padding:0 6px;
 -webkit-mask-image:linear-gradient(90deg,transparent,#000 12px);mask-image:linear-gradient(90deg,transparent,#000 12px)}
.wave i{width:2px;border-radius:1px;background:#f5f5f7;flex:none}
.wave.dead{-webkit-mask-image:none;mask-image:none}
.wave.dead b{display:block;width:100%;height:2px;border-radius:1px;background:rgba(240,68,56,.8)}
.big{flex-direction:column;align-items:stretch;height:auto;border-radius:20px;padding:4px;min-width:360px}
.big .top{display:flex;align-items:center}
.words{border-top:1px solid rgba(255,255,255,.09);margin:4px 2px 0;padding:6px 10px 8px;font-size:13px;line-height:17px;color:rgba(255,255,255,.92)}
.words .old{color:rgba(255,255,255,.55)}
.mini{display:flex;gap:10px;flex-wrap:wrap}
.mini .pill{height:40px}
.mini .lbl2{font-size:11px;color:#1f1f1f;margin-top:4px;text-align:center}
"""
def lead(): return '<span class="chip"><span class="dot"></span><span class="clock">0:12</span></span>'
TAIL = '<span class="div"></span><span class="chip cancel">Cancel</span><span class="chip finish">Finish</span>'
cap_body = f'''<div class="scroll"><div class="app desk">
<div class="menubar"><span><b>Safari</b> File Edit View</span><span>✎ &nbsp;Thu 2:41 PM</span></div>
<div class="stage">
 <div class="lane"><div class="lbl"><b>Today</b>Red dot and clock only</div>
  <div class="pill">{lead()}{TAIL}</div></div>
 <div class="lane"><div class="lbl"><b>cp2 default</b>Waveform on, from PR #114</div>
  <div class="pill">{lead()}{wave("talk")}{TAIL}</div></div>
 <div class="lane"><div class="lbl"><b>Live words on</b>Setting, off by default</div>
  <div class="pill big"><div class="top">{lead()}{wave("talk")}{TAIL}</div>
  <div class="words"><span class="old">so this button here should open the</span><br>settings but it opens the history instead</div></div></div>
 <div class="lane"><div class="lbl"><b>Waveform states</b>What the mic is doing</div>
  <div class="mini">
   <div><div class="pill">{lead()}{wave("talk")}</div><div class="lbl2">Talking</div></div>
   <div><div class="pill">{lead()}{wave("quiet")}</div><div class="lbl2">Silence: dim dots</div></div>
   <div><div class="pill">{lead()}{wave("dead")}</div><div class="lbl2">No audio for 600 ms</div></div>
  </div></div>
</div></div></div>
<p class="cap"><b>Recording capsule.</b> It sits at the top center of the screen, under the menu bar. The waveform is 96 pt wide and scrolls left, newest bar on the right. A flat red line means the mic sends no audio.</p>'''
(out/"10-capsule.html").write_text(page("Recording Capsule", CAP_CSS, cap_body))

# ---------- 2. Shortcuts pane ----------
sc_body = f'''<div class="scroll"><div class="app win set">{sidebar("Shortcuts")}
<div class="main"><h2>Shortcuts</h2>
<div class="hd">Start and finish</div>
<div class="grp">
 <div class="row"><div>Record<small>Press again to finish.</small></div><span class="kbd">⌥⇧S</span></div>
 <div class="row"><div>Paste last capture</div><span class="kbd">⌥⇧V <span class="x">✕</span></span></div>
</div>
<div class="hd">While recording</div>
<div class="grp">
 <div class="row"><div>Draw<small>Hold and move for the laser. Hold and drag for ink. Hold and click to drop a pin.</small></div><span class="pop">⇧ Shift</span></div>
 <div class="row"><div>Undo stroke<small>Works only while you record.</small></div><span class="kbd">⌘Z</span></div>
 <div class="row"><div>Cancel recording<small>Stops and throws the recording away.</small></div><span class="pop">Esc</span></div>
</div>
<div class="hd">When a key is taken</div>
<div class="grp">
 <div class="row"><div>Paste last capture<div class="err">⌥⇧S is already Record. Pick another key.</div></div><span class="kbd" style="box-shadow:inset 0 0 0 1.5px #ff6b6b">⌥⇧S</span></div>
</div>
</div></div></div>
<p class="cap"><b>Settings, Shortcuts pane (cp4).</b> The dashed item is new. Record and Paste move here from General. Draw takes one modifier key, and Cancel takes Esc or F1 to F12.</p>'''
(out/"20-shortcuts.html").write_text(page("Shortcuts Pane", SETTINGS_CSS, sc_body))

# ---------- 3. Recording pane ----------
meter = '<span style="display:inline-flex;gap:2px">'+"".join(f'<i style="width:6px;height:10px;border-radius:2px;background:{"#30d158" if i<7 else "rgba(255,255,255,.15)"}"></i>' for i in range(14))+'</span>'
rec_body = f'''<div class="scroll"><div class="app win set">{sidebar("Recording")}
<div class="main"><h2>Recording</h2>
<div class="grp">
 <div class="row"><div>Microphone</div><span class="pop">MacBook Pro Microphone</span></div>
 <div class="row"><div>Input level</div>{meter}</div>
</div>
<div class="hd">Transcription</div>
<div class="grp">
 <div class="row new"><div>Engine<small>Falls back to Apple Speech when Deepgram fails or has no key.</small></div><span class="pop">Deepgram nova-3</span></div>
 <div class="row"><div>Language</div><span class="pop">Auto-detect</span></div>
</div>
<div class="hd">While recording</div>
<div class="grp">
 <div class="row"><div>Show live audio<small>The waveform in the capsule.</small></div><span class="sw on"></span></div>
 <div class="row new"><div>Show live words<small>Apple Speech on macOS 26. Adds two lines under the capsule.</small></div><span class="sw"></span></div>
</div>
<div class="hd">For agents</div>
<div class="grp"><div class="row new"><div>Save app state<small>Saves what is under each click and mark, and the window's text at each keyframe. It stays with the capture, also after the video is cleaned.</small></div><span class="sw on"></span></div></div>
<div class="hd">Experimental</div>
<div class="grp"><div class="row"><div>Video export</div><span class="sw"></span></div></div>
</div></div></div>
<p class="cap"><b>Settings, Recording pane (cp2 and cp3).</b> Deepgram is the default engine. Live audio is on, live words are off, and Save app state (cp6) is on. Storage keeps recordings Forever, and About loses Check for Updates.</p>'''
(out/"30-recording.html").write_text(page("Recording Pane", SETTINGS_CSS, rec_body))

# ---------- 4. History ----------
HIST_CSS = """
.wrap{display:grid;grid-template-columns:200px 1fr;gap:16px;align-items:start;min-width:620px}
.menu{background:rgba(40,40,42,.97);border-radius:9px;padding:5px;box-shadow:0 0 0 1px rgba(255,255,255,.1),0 8px 24px rgba(0,0,0,.25);font-size:13px}
.mi{display:flex;justify-content:space-between;padding:3px 9px;border-radius:5px}.mi span{color:var(--a-dim)}
.mi.sel{background:var(--a-switch)}.mi.sel span{color:#fff}
.sep{height:1px;background:rgba(255,255,255,.1);margin:5px 9px}
.mi.gone{color:var(--a-faint);text-decoration:line-through}
.hist .bar{display:flex;align-items:center;gap:14px;height:44px;padding:0 14px;border-bottom:1px solid var(--a-line)}
.hist .bar b{font-size:13px;font-weight:600}.hist .bar span{color:var(--a-dim);margin-left:auto;font-size:12px}
table{width:100%;border-collapse:collapse;font-size:12.5px}
th{text-align:left;font-weight:500;color:var(--a-dim);font-size:11.5px;padding:7px 14px;border-bottom:1px solid var(--a-line)}
td{padding:7px 14px;white-space:nowrap}
tr:nth-child(even) td{background:rgba(255,255,255,.025)}
tr.sel td{background:var(--a-switch)}
td.len{font-variant-numeric:tabular-nums}
.vid{font-size:11px;padding:1px 7px;border-radius:9px;background:rgba(105,216,159,.18);color:var(--a-green)}
.vid.off{background:rgba(255,255,255,.08);color:var(--a-faint)}
.apps{position:relative}.more{font-size:11px;padding:0 5px;border-radius:7px;background:rgba(255,255,255,.22)}.more:empty{display:none}
.tip{position:absolute;left:14px;top:30px;z-index:2;background:#3a3a3c;color:#fff;font-size:11.5px;padding:4px 8px;border-radius:6px;box-shadow:0 4px 12px rgba(0,0,0,.4)}
tr.sel .vid{background:rgba(255,255,255,.25);color:#fff}
"""
rows=[("Nov 13, 2:41 PM","Safari → T3 Code → Finder","+1","0:42",1),("Nov 13, 11:05 AM","T3 Code","","1:18",1),
      ("Nov 12, 4:30 PM","Figma → Safari","","3:07",1),("Nov 10, 9:12 AM","iPhone","","0:24",1),("Oct 28, 3:55 PM","Xcode","","0:18",0)]
TIP='<span class="tip">Safari → T3 Code → Finder → Xcode</span>'
tr="".join(f'<tr class="{"sel" if i==0 else ""}"><td>{d}</td><td class="apps">{a} <span class="more">{w}</span>{TIP if i==0 else ""}</td><td class="len">{l}</td><td><span class="vid{"" if v else " off"}">{"Video" if v else "Video cleaned"}</span></td></tr>' for i,(d,a,w,l,v) in enumerate(rows))
hist_body = f'''<div class="scroll"><div class="app wrap">
<div class="menu">
 <div class="mi">Record Screen<span>⌥⇧S</span></div><div class="mi">Choose What to Record…</div><div class="sep"></div>
 <div class="mi">Open Latest Capture</div><div class="mi sel new">History…</div><div class="sep"></div>
 <div class="mi">Settings…<span>⌘,</span></div><div class="mi gone">Check for Updates…</div><div class="mi">Quit Annotator<span>⌘Q</span></div>
</div>
<div class="win hist"><div class="bar"><div class="lights"><i></i><i></i><i></i></div><b>History</b><span>Newest first</span></div>
<table><tr><th>Date</th><th>Apps</th><th>Length</th><th>Video</th></tr>{tr}</table></div>
</div></div>
<p class="cap"><b>History window (cp5).</b> The menu bar menu gets <b>History…</b> and loses Check for Updates. A Screen-mode capture lists every app that was in front, and the tooltip shows all of them. Double-click a row to open it in the Review window. Captures stay forever, so an old row shows “Video cleaned” only when its video file is gone.</p>'''
(out/"40-history.html").write_text(page("History Window", HIST_CSS, hist_body))

# ---------- 5. Review with Video ----------
REV_CSS = """
.rev{min-width:640px;position:relative}
.rh{display:flex;align-items:center;gap:12px;height:44px;padding:0 12px;background:#111;border-bottom:1px solid #2a2a2a;font-size:12px}
.seg{display:inline-flex;background:rgba(255,255,255,.07);border-radius:8px;padding:2px;height:24px}
.seg span{padding:0 10px;display:flex;align-items:center;border-radius:6px;color:var(--a-dim)}
.seg span.on{background:rgba(255,255,255,.16);color:var(--a-text)}
.meta{color:#969696}.rh .r{margin-left:auto;display:flex;gap:8px}
.btn{height:24px;padding:0 10px;display:flex;align-items:center;border-radius:6px;background:rgba(255,255,255,.08)}
.btn.blue{background:var(--a-accent)}
.rb{display:grid;grid-template-columns:1fr 34%;background:#161616;min-height:300px}
.st{padding:16px;display:grid;place-items:center}
.player{width:100%;aspect-ratio:16/10;border-radius:10px;overflow:hidden;position:relative;background:linear-gradient(160deg,#2c3a55,#1b2233 60%,#3a2a3f);box-shadow:0 0 0 1px rgba(255,255,255,.12)}
.player .shot{position:absolute;inset:10% 8% 22%;border-radius:6px;background:#f4f4f2;opacity:.9}
.player .shot:before{content:"";position:absolute;left:8%;top:12%;width:45%;height:8%;background:#d0d4dc;border-radius:3px;box-shadow:0 18px 0 #e2e5ea,0 36px 0 #e2e5ea}
.ink{position:absolute;left:52%;top:30%;width:22%;height:22%;border:3px solid #ff3b30;border-radius:50%;transform:rotate(-8deg)}
.ctl{position:absolute;left:50%;bottom:12px;transform:translateX(-50%);width:72%;display:flex;align-items:center;gap:10px;padding:7px 12px;border-radius:12px;background:rgba(30,30,30,.72);backdrop-filter:blur(8px);font-size:11px;color:#eee}
.ctl .tr{flex:1;height:4px;border-radius:2px;background:rgba(255,255,255,.25);position:relative}
.ctl .tr:before{content:"";position:absolute;left:0;top:0;bottom:0;width:29%;background:#fff;border-radius:2px}
.sb{background:#111;border-left:1px solid #2a2a2a;padding:14px;font-size:12px}
.sb h5{margin:0 0 8px;font-size:12px;font-weight:600}
.tbox{background:#1d1d1d;border-radius:8px;padding:10px;line-height:1.5;color:#ddd;margin-bottom:14px}
.tbox mark{background:rgba(59,92,246,.35);color:#fff;border-radius:3px;padding:0 2px}
.tl div{display:flex;gap:8px;padding:5px 0;border-top:1px solid #2a2a2a;color:#ccc}.tl div span{color:#969696;font-variant-numeric:tabular-nums;width:34px}
"""
rev_body = f'''<div class="scroll"><div class="app win rev">
<div class="rh"><div class="lights"><i></i><i></i><i></i></div>
 <div class="seg"><span>Contact Sheet</span><span>Keyframes</span><span class="on new">Video</span></div>
 <span class="meta">Safari → T3 Code → Finder → Xcode · 0:42 · 6 keyframes</span>
 <div class="r"><span class="btn">Show in Finder</span><span class="btn blue">Copy</span></div></div>
<div class="rb"><div class="st"><div class="player"><div class="shot"></div><div class="ink"></div>
 <div class="ctl">▶<span>0:12</span><span class="tr"></span><span>0:42</span></div></div></div>
 <div class="sb"><h5>Transcript</h5><div class="tbox">So this button here should open the settings, but it opens the history instead. Can we fix that before the release?</div>
 <h5>Timeline</h5><div class="tl"><div><span>0:04</span>Keyframe 1</div><div><span>0:12</span>Ink circle</div><div><span>0:19</span>Keyframe 2</div><div><span>0:31</span>Pin 1</div></div></div></div>
</div></div>
<p class="cap"><b>Review window, Video tab (cp5).</b> The tab plays the capture's video with the system player (AVPlayerView, from the helper). The transcript and timeline stay on the right, as they do today.</p>'''
(out/"50-review-video.html").write_text(page("Review Video Tab", REV_CSS, rev_body))
print("built", sorted(p.name for p in out.iterdir()))

# ---------- 6. App shot in the terminal ----------
TERM_CSS = """
.term{background:#0f1115;border-radius:10px;box-shadow:0 0 0 1px rgba(255,255,255,.1);overflow:hidden;min-width:600px}
.tbar{display:flex;align-items:center;gap:12px;height:30px;padding:0 12px;background:#1b1e24;color:rgba(255,255,255,.55);font-size:12px}
pre{overflow-x:auto;margin:0;padding:12px 14px 14px;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px;line-height:1.55;color:#d6d9df;white-space:pre}
.p{color:#7ee787}.c{color:#79c0ff}.d{color:#8b949e}.w{color:#e3b341}.h{color:#ffffff;font-weight:600}
.lbl{font-size:12px;color:var(--ds-color-text-muted);margin:12px 0 6px}
.lbl:first-child{margin-top:0}
"""
term_body = '''<div class="scroll app">
<div class="lbl">What Ford pastes into the agent</div>
<div class="term"><pre><span class="h">[scrawl 0:42 · Safari “Fixture” · ~/…/captures/scrawl-1794586221550-4f2a.scrawl]</span>
<span class="w">App shots: scrawl appshot scrawl-1794586221550-4f2a --frame &lt;keyframe&gt;</span>
- #1 (0:04) so this button here should open the settings,
- #2 (0:19) but it opens the history instead.
- #3 (0:31) and here in Xcode is the helper that draws it.</pre></div>
<div class="lbl">What the agent runs when it wants the app state</div>
<div class="term"><div class="tbar"><div class="lights"><i></i><i></i><i></i></div>zsh</div><pre><span class="p">$</span> <span class="c">scrawl appshot scrawl-1794586221550-4f2a</span>
<span class="d"> #  time  app      window          url                  age    nodes</span>
 1  0:04  Safari   Fixture         …/appshot-secret.html  0.3 s    212
 2  0:19  T3 Code  Annotator plan  -                      0.8 s  1,500 <span class="w">cut: nodes</span>
 3  0:31  Xcode    Player.swift    -                      1.1 s    904

<span class="p">$</span> <span class="c">scrawl appshot scrawl-1794586221550-4f2a --frame 1</span>
<span class="h">Safari · “Fixture” · …/appshot-secret.html · 0:04 · 212 nodes</span>
window "Fixture"
  group
    heading "Settings"
    static text "Open the settings to change your keys."
    text field "Name"
    button "Save"</pre></div>
</div>
<p class="cap"><b>App shot (cp6).</b> The pasted text gets one line with the real capture id. The agent lists the shots, then reads one keyframe's window text. A text field shows its name, never its value, and the URL loses its query.</p>'''
(out/"60-appshot.html").write_text(page("App Shot CLI", TERM_CSS, term_body))
print("built 60")
