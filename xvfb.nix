# Xvfb whose XTEST pointer has scroll axes, so starfold can inject smooth
# (pixel-precise) scrolling. Without it starfold falls back to wheel clicks.
{xorg}:
xorg.xorgserver.overrideAttrs (old: {
  patches = (old.patches or []) ++ [./patches/xorg-server-xtest-scroll.patch];
})
