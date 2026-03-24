--track@level:調整幅,0,1,1,0.0001
--[[pixelshader@mult_pixels:
Texture2D<float4> Input : register(t0);
cbuffer Constants : register(b0)
{
  float val;
};

float4 mult_pixels(float4 pos: SV_Position) : SV_Target
{
    float4 pixel = Input.Load(int3(pos.xy, 0));
    return float4(pixel.rgb * val, pixel.a);
}
]]

obj.pixelshader("mult_pixels", "object", "object", { level })
