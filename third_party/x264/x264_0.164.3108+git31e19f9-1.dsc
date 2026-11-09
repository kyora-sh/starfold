-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA512

Format: 3.0 (quilt)
Source: x264
Binary: x264, libx264-164, libx264-dev
Architecture: any
Version: 2:0.164.3108+git31e19f9-1
Maintainer: Debian Multimedia Maintainers <debian-multimedia@lists.debian.org>
Uploaders:  Reinhard Tartler <siretart@tauware.de>, Rico Tzschichholz <ricotz@ubuntu.com>
Homepage: https://www.videolan.org/developers/x264.html
Standards-Version: 4.6.1
Vcs-Browser: https://salsa.debian.org/multimedia-team/x264
Vcs-Git: https://salsa.debian.org/multimedia-team/x264.git
Testsuite: autopkgtest
Testsuite-Triggers: ffmpeg
Build-Depends: debhelper-compat (= 13), libavformat-dev (>= 6:9) <!stage1>, libffms2-dev <!stage1>, nasm (>= 2.13) [any-i386 any-amd64], pkg-config
Package-List:
 libx264-164 deb libs optional arch=any
 libx264-dev deb libdevel optional arch=any
 x264 deb graphics optional arch=any profile=!stage1
Checksums-Sha1:
 5c7733d7122f3a76d88235c635e2ddd8233643a0 954789 x264_0.164.3108+git31e19f9.orig.tar.gz
 a72dcb642628d9a73663458795fb5a063c0f59d2 23648 x264_0.164.3108+git31e19f9-1.debian.tar.xz
Checksums-Sha256:
 41606cb8e788a7f8c4514290646d4ba5c7bc68d9e1ccd1a73f446a90546913eb 954789 x264_0.164.3108+git31e19f9.orig.tar.gz
 7844a9b42bedec211bc9a131d0655ea52524fbb055582921d8e6d4a4d953efc6 23648 x264_0.164.3108+git31e19f9-1.debian.tar.xz
Files:
 29db8354468253ce22a8344d076c9b89 954789 x264_0.164.3108+git31e19f9.orig.tar.gz
 010d67567d8b6f9deb1ed199be292b1c 23648 x264_0.164.3108+git31e19f9-1.debian.tar.xz

-----BEGIN PGP SIGNATURE-----

iHUEARYKAB0WIQRCYn6EHZln2oPh+pAhk2s2YA/NiQUCZbbMSAAKCRAhk2s2YA/N
iWuLAQCHRv9B8idd8VENBiGt1YLQPTWlgm8ST1LzJ7qOGgYxKgEA0K+XiD7Pkiwt
Pt3bpVX4JKXo+1lRtm1WOiplHdA6ZQA=
=JBmc
-----END PGP SIGNATURE-----
