import json,sys
path=sys.argv[1]; maxres=int(sys.argv[2]) if len(sys.argv)>2 else 600
for i,line in enumerate(open(path)):
    try: o=json.loads(line)
    except Exception: continue
    t=o.get('type')
    if t not in ('user','assistant'): continue
    msg=o.get('message',{})
    c=msg.get('content')
    ts=o.get('timestamp','')
    if isinstance(c,str):
        print(f"--- [{i}] {t} {ts}\n{c[:4000]}")
        continue
    for part in c or []:
        pt=part.get('type')
        if pt=='text':
            print(f"--- [{i}] {t} TEXT {ts}\n{part['text'][:6000]}")
        elif pt=='tool_use':
            inp=json.dumps(part.get('input'))
            print(f"--- [{i}] {t} TOOL_USE {part.get('name')} {ts}\n{inp[:3000]}")
        elif pt=='tool_result':
            cc=part.get('content')
            if isinstance(cc,list):
                s=' '.join(x.get('text','') for x in cc if isinstance(x,dict))
            else: s=str(cc)
            print(f"--- [{i}] RESULT\n{s[:maxres]}")
        elif pt=='thinking':
            th=part.get('thinking','')
            if th: print(f"--- [{i}] THINK\n{th[:2500]}")
