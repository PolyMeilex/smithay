#version 100
#extension GL_OES_EGL_image_external : require

precision mediump float;
uniform samplerExternalOES tex;
uniform float alpha;
varying vec2 v_tex_coords;

void main() {
    gl_FragColor = texture2D(tex, v_tex_coords) * alpha;
}