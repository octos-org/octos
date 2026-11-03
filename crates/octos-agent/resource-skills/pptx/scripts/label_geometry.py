#!/usr/bin/env python3
"""Label-plane geometry in canvas pixels. Measurement/proposals, not visual approval.

Quads are ordered local top-left, top-right, bottom-right, bottom-left. Screen
coordinates have y down, so positive rotation is clockwise, as in PowerPoint.
"""
import argparse
import json
import math


def finite(value):
    return type(value) in (int, float) and math.isfinite(value)


def rotation(value):
    if not finite(value) or not -360 < value < 360:
        raise ValueError('rotation must be finite degrees in (-360, 360)')
    return value


def angle_delta(a, b):
    return (a-b+180) % 360-180


def rotate(point, center, degrees):
    theta = math.radians(degrees); c, s = math.cos(theta), math.sin(theta)
    x, y = point[0]-center[0], point[1]-center[1]
    return [center[0]+c*x-s*y, center[1]+s*x+c*y]


def frame_quad(box, degrees=0):
    x, y, w, h = box
    center = [x+w/2, y+h/2]
    return [rotate(p, center, degrees) for p in [[x,y],[x+w,y],[x+w,y+h],[x,y+h]]]


def cross(a, b, p):
    return (b[0]-a[0])*(p[1]-a[1])-(b[1]-a[1])*(p[0]-a[0])


def valid_quad(q):
    if (not isinstance(q, list) or len(q) != 4 or
            any(not isinstance(p, list) or len(p) != 2 or not all(map(finite,p)) for p in q)):
        raise ValueError('quad must contain four finite [x,y] points in TL,TR,BR,BL order')
    if any(cross(q[i],q[(i+1)%4],q[(i+2)%4]) <= 1e-8 for i in range(4)):
        raise ValueError('quad must be convex, nondegenerate and clockwise in screen coordinates')
    return q


def bounds(q):
    x, y = min(p[0] for p in q), min(p[1] for p in q)
    return [x,y,max(p[0] for p in q)-x,max(p[1] for p in q)-y]


def contains(q, points, tolerance=0):
    return all(cross(a,b,p) >= -tolerance*math.dist(a,b)-1e-8
               for a,b in zip(q,q[1:]+q[:1]) for p in points)


def area(q):
    return abs(sum(a[0]*b[1]-a[1]*b[0] for a,b in zip(q,q[1:]+q[:1])))/2


def plane(q):
    """Best edge-aligned rigid plane plus a residual; no silent projective fit."""
    valid_quad(q)
    u=[(q[1][i]-q[0][i]+q[2][i]-q[3][i])/2 for i in [0,1]]
    v=[(q[3][i]-q[0][i]+q[2][i]-q[1][i])/2 for i in [0,1]]
    w=math.hypot(*u); h=math.hypot(*v)
    deg=math.degrees(math.atan2(u[1],u[0]))
    center=[sum(p[i] for p in q)/4 for i in [0,1]]
    box=[center[0]-w/2,center[1]-h/2,w,h]
    error=max(math.dist(a,b) for a,b in zip(q,frame_quad(box,deg)))
    return {'rotation':deg,'box':box,'rigid_fit_error_px':error,
            'rigid_fit_relative_error':error/min(w,h),
            'kind':'perspective_or_shear' if error/min(w,h) > .03 else 'approximately_rigid'}


def region(value, canvas=None):
    """Legacy rectangles remain valid; explicit label quads carry their evidence."""
    if isinstance(value, list):
        if len(value)!=4 or not all(map(finite,value)) or min(value[2:])<=0:
            raise ValueError('region must be [x,y,width,height] with positive size')
        data={'quad':frame_quad(value),'scope':'unspecified','basis':'Legacy rectangle; label ownership unverified'}
    elif isinstance(value, dict):
        allowed={'quad','scope','basis','baseline_deg','angle_tolerance_deg','source_quad'}
        if set(value)-allowed:
            raise ValueError('Unsupported region fields: '+str(set(value)-allowed))
        if value.get('scope') not in ('label','panel') or not isinstance(value.get('basis'),str) or not value['basis'].strip():
            raise ValueError('Quad region needs scope label/panel and an artwork measurement basis')
        data=dict(value)
        valid_quad(data.get('quad'))
        if 'source_quad' in data: valid_quad(data['source_quad'])
    else:
        raise ValueError('Region must be a rectangle or an evidenced quad')
    q=valid_quad(data['quad'])
    for candidate in [q]+([data['source_quad']] if 'source_quad' in data else []):
        if canvas and any(not 0<=p[0]<=canvas['width_px'] or not 0<=p[1]<=canvas['height_px'] for p in candidate):
            raise ValueError('Region lies outside canvas')
    fit=plane(q)
    if 'baseline_deg' in data: rotation(data['baseline_deg'])
    tolerance=data.get('angle_tolerance_deg',2)
    if not finite(tolerance) or not 0<=tolerance<=180:
        raise ValueError('angle_tolerance_deg must be finite in [0,180]')
    return data | {'box':bounds(q),'plane':fit,'angle_tolerance_deg':tolerance,
                   'baseline_deg':data.get('baseline_deg',fit['rotation'])}


def propose(q, padding=4):
    """Inscribed rotated rectangle; does not choose font, copy or approve placement."""
    if not finite(padding) or padding<0: raise ValueError('padding must be nonnegative')
    p=plane(q); x,y,w,h=p['box']; center=[x+w/2,y+h/2]
    if not contains(q,[center],-padding): raise ValueError('Padding consumes label')
    lo,hi=0.,1.
    for _ in range(50):
        scale=(lo+hi)/2
        box=[center[0]-w*scale/2,center[1]-h*scale/2,w*scale,h*scale]
        if contains(q,frame_quad(box,p['rotation']),-padding):lo=scale
        else:hi=scale
    return {'box':[center[0]-w*lo/2,center[1]-h*lo/2,w*lo,h*lo],
            'rotation':p['rotation'],'plane':p,'visual_review_required':True,
            'limitation':('Native rotation cannot reproduce this projective/sheared plane; inspect and choose an editable treatment.'
                          if p['kind']=='perspective_or_shear' else None)}


def outside_ink(components, q, image_size, canvas, tolerance_px=0):
    """Count attributable raster pixels outside a polygon, not its bounding box."""
    sx=canvas['width_px']/image_size[0]; sy=canvas['height_px']/image_size[1]
    count=0
    for component in components:
        for y,x0,x1 in component['runs']:
            # A convex polygon intersects a scanline in one interval.
            yy=(y+.5)*sy; lo,hi=-math.inf,math.inf
            for a,b in zip(q,q[1:]+q[:1]):
                dx,dy=b[0]-a[0],b[1]-a[1]
                rhs=dx*(yy-a[1])+dy*a[0]+tolerance_px*math.hypot(dx,dy)
                if abs(dy)<1e-10:
                    if rhs < -1e-8: lo,hi=1,0; break
                elif dy>0: hi=min(hi,rhs/dy)
                else: lo=max(lo,rhs/dy)
            start=max(x0,math.ceil(lo/sx-.5)) if math.isfinite(lo) else x0
            end=min(x1,math.floor(hi/sx-.5)+1) if math.isfinite(hi) else x1
            count+=(x1-x0)-max(0,end-start)
    return count


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--quad',required=True,help='JSON array of four canvas-pixel corners')
    parser.add_argument('--padding',type=float,default=4)
    parser.add_argument('--out',help='Optional JSON proposal path; never edits page data')
    args=parser.parse_args()
    result=propose(json.loads(args.quad),args.padding)
    text=json.dumps(result,ensure_ascii=False,indent=2)
    if args.out:
        from pathlib import Path
        Path(args.out).write_text(text+'\n')
    print(text)


if __name__=='__main__': main()
