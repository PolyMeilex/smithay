#version 100

precision mediump float;
uniform sampler2D tex;
uniform float alpha;
varying vec2 v_tex_coords;

void main() {
    gl_FragColor = vec4(texture2D(tex, v_tex_coords).rgb, 1.0) * alpha;
}