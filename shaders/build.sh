#!/bin/sh
# Regenerates the committed SPIR-V. Only needed after editing a shader, so
# building the client never requires a shader compiler.
set -e
cd "$(dirname "$0")"
glslc -O --target-env=vulkan1.0 ui.vert -o ui.vert.spv
glslc -O --target-env=vulkan1.0 ui.frag -o ui.frag.spv
spirv-val ui.vert.spv && spirv-val ui.frag.spv
