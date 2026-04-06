struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) in_vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;

    // Fullscreen triangle
    let x = f32((in_vertex_index << 1u) & 2u);
    let y = f32(in_vertex_index & 2u);

    out.clip_position = vec4<f32>(
        x * 2.0 - 1.0,
        -(y * 2.0 - 1.0),
        0.0,
        1.0
    );

    out.tex_coords = vec2<f32>(x, y);
    return out;
}

// YUV planes
@group(0) @binding(0) var t_y: texture_2d<f32>;
@group(0) @binding(1) var s_y: sampler;

@group(0) @binding(2) var t_u: texture_2d<f32>;
@group(0) @binding(3) var s_u: sampler;

@group(0) @binding(4) var t_v: texture_2d<f32>;
@group(0) @binding(5) var s_v: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(t_y, s_y, in.tex_coords).r;
    let u = textureSample(t_u, s_u, in.tex_coords).r - 0.5;
    let v = textureSample(t_v, s_v, in.tex_coords).r - 0.5;

    // Коэффициенты BT.709
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;

    let rgb = vec3<f32>(r, g, b);
    
    // ПРИМЕНЯЕМ ГАММУ 2.2 (делает цвета сочнее и темнее)
    return vec4<f32>(pow(max(rgb, vec3<f32>(0.0)), vec3<f32>(2.2)), 1.0);
}