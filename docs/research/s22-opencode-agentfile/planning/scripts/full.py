import json,sys
path=sys.argv[1]; want=set(int(x) for x in sys.argv[2].split(','))
for i,line in enumerate(open(path)):
    if i not in want: continue
    o=json.loads(line)
    c=o.get('message',{}).get('content')
    for part in c or []:
        if part.get('type')=='tool_result':
            cc=part.get('content')
            s=' '.join(x.get('text','') for x in cc if isinstance(x,dict)) if isinstance(cc,list) else str(cc)
            print(f"=== [{i}]\n{s}")
