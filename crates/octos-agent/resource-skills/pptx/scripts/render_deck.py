#!/usr/bin/env python3
"""Render a real PPTX into a fresh PDF, page PNGs and a contact sheet."""
import argparse,json,os,shutil,subprocess,hashlib,sys
from pathlib import Path
import pymupdf as fitz
from PIL import Image,ImageDraw,ImageFont
from pptx import Presentation
def main():
 ap=argparse.ArgumentParser();ap.add_argument('pptx',type=Path);ap.add_argument('--out',type=Path,required=True);ap.add_argument('--size',default='3840x2160');ap.add_argument('--soffice');ap.add_argument('--pages',help='Comma-separated 1-based pages to rasterize; PDF conversion remains whole-deck');a=ap.parse_args()
 src=a.pptx.resolve();dest=a.out.resolve();dest.mkdir(parents=True,exist_ok=True)
 width,height=map(int,a.size.split('x'));p=Presentation(src);expected=len(p.slides)
 selected=sorted(set(map(int,a.pages.split(',')))) if a.pages else list(range(1,expected+1))
 if not selected or selected[0]<1 or selected[-1]>expected:raise SystemExit('Requested pages outside deck')
 if width<=0 or height<=0:raise SystemExit('Export dimensions must be positive.')
 if abs((width/height)/(p.slide_width/p.slide_height)-1)>0.001:raise SystemExit('Export aspect ratio must match the deck; do not stretch artwork.')
 soffice=a.soffice or shutil.which('soffice') or '/Applications/LibreOffice.app/Contents/MacOS/soffice'
 if not Path(soffice).exists():raise SystemExit('No renderer found; pass --soffice.')
 pdf=dest/(src.stem+'.pdf')
 if pdf.exists():raise SystemExit('Use a fresh render directory; existing PDF was preserved.')
 cache=dest/'.cache';cache.mkdir(exist_ok=True)
 fc=dest/'fonts.conf';fc.write_text('''<?xml version="1.0"?><!DOCTYPE fontconfig SYSTEM "fonts.dtd"><fontconfig><dir>/System/Library/Fonts</dir><dir>/Library/Fonts</dir><dir prefix="xdg">fonts</dir><dir>~/Library/Fonts</dir><dir>/Applications/Microsoft PowerPoint.app/Contents/Resources/DFonts</dir><dir>/Applications/LibreOffice.app/Contents/Resources/fonts</dir><cachedir>'''+str(cache)+'''</cachedir></fontconfig>''')
 env=dict(os.environ,XDG_CACHE_HOME=str(cache))
 if sys.platform=='darwin':env['FONTCONFIG_FILE']=str(fc)
 cmd=[soffice,'-env:UserInstallation='+ (dest/'.lo-profile').as_uri(),'--headless','--convert-to','pdf','--outdir',str(dest),str(src)]
 run=subprocess.run(cmd,env=env,capture_output=True,text=True,timeout=180)
 (dest/'render.log').write_text(run.stdout+run.stderr)
 if run.returncode or not pdf.exists():raise SystemExit('Renderer failed; inspect render.log.')
 doc=fitz.open(pdf)
 if len(doc)!=expected:raise SystemExit(f'Page mismatch: {len(doc)} != {expected}')
 images=dest/'png';images.mkdir(exist_ok=True)
 cols=min(3,len(selected));rows=(len(selected)+cols-1)//cols;sheet=Image.new('RGB',(cols*640,rows*390),'#e7e3dd');dr=ImageDraw.Draw(sheet)
 fonts={}
 page_images=[]
 for position,i in enumerate(selected):
  page=doc[i-1]
  pix=page.get_pixmap(matrix=fitz.Matrix(width/page.rect.width,height/page.rect.height),alpha=False)
  if (pix.width,pix.height)!=(width,height):raise SystemExit('Unexpected output size.')
  path=images/f'slide-{i:02}.png';pix.save(path);im=Image.open(path);im.thumbnail((630,354),Image.Resampling.LANCZOS)
  x=(position%cols)*640;y=(position//cols)*390;sheet.paste(im,(x+5,y+27));dr.text((x+8,y+6),f'P{i:02}',fill='black')
  page_images.append({'page':i,'path':str(path.relative_to(dest)),'sha256':hashlib.sha256(path.read_bytes()).hexdigest()})
  for block in page.get_text('dict')['blocks']:
   for line in block.get('lines',[]):
    for sp in line['spans']:fonts[sp['font']]=fonts.get(sp['font'],0)+1
 sheet.save(dest/'contact-sheet.jpg',quality=95)
 report={'input_sha256':hashlib.sha256(src.read_bytes()).hexdigest(),'pdf_sha256':hashlib.sha256(pdf.read_bytes()).hexdigest(),'pages':expected,'rendered_pages':selected,'page_images':page_images,'whole_deck_pdf_conversion':True,'export_pixels':[width,height],'source_native_resolution':'Not inferred from export size','rendered_fonts':fonts,'renderer':soffice}
 (dest/'render-report.json').write_text(json.dumps(report,indent=2));print(json.dumps(report,indent=2))
if __name__=='__main__':main()
