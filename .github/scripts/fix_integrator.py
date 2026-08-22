from pathlib import Path

path = Path('.github/scripts/integrate_satellite_v2.py')
text = path.read_text(encoding='utf-8')
old = '''detail_pattern = re.compile(
    r''' + "'''" + r'''            ui\.label\("Detail"\);\n            ComboBox::from_id_salt\("rw-ui-sat-downsample"\).*?                \);\n''' + "'''" + ''',
    re.S,
)
'''
new = '''detail_pattern = re.compile(
    r''' + "'''" + r'''            ui\.label\("Detail"\);\n            ComboBox::from_id_salt\("rw-ui-sat-downsample"\).*?                \);\n(?=        \}\);\n\n        ui\.horizontal)''' + "'''" + ''',
    re.S,
)
'''
if old not in text:
    raise SystemExit('bad detail-pattern definition was not found')
path.write_text(text.replace(old, new, 1), encoding='utf-8')
print('tightened satellite detail-control replacement')
