# Running Borland C++ 2.0

One can run the Borland C compiler by using this command:

```
npm exec -p @rawrs/borland-c-2 bcc
```

One can run it against a C++ file, in this case `FOO.CPP` in the current working directory by using this command:

```
npm exec -p @rawrs/borlnd-c-2 bcc FOO.CPP
```

Which will perform the default option set. We have determined that the default options are equivalent to the following:

```
npm exec -p @rawrs/borlnd-c-2 bcc -ms -p- -k -V -Z -O -r -G -ID:\\BC2\\INCLUDE -LD:\\BC2\\LIB FOO.CPP
```

You can specify all of these options just by including them, and it will automatically use the -I and -L to point to the internal paths for the standard includes and libraries:

```
npm exec -p @rawrs/borland-c-2 bcc -ms -p- -k -V -Z -O -r -G FOO.CPP
```

If you want to completely use your own and omit these, specify `--no-system-includes` and/or `--no-system-libs`

You can build a CPP file and not link it via -S for just assembly:

```
npm exec -p @rawrs/borland-c-2 bcc -ms -p- -k -V -Z -O -r -G -S MAIN.CPP
```

which creates MAIN.ASM. If you want just the object file, use -c:

```
npm exec -p @rawrs/borland-c-2 bcc -ms -p- -k -V -Z -O -r -G -c MAIN.CPP
```

Which creates an OBJ file with the same name as the CPP.

Then to link that MAIN.OBJ file to create an executable, use TLINK. Run it alone to see the options:

```
npm exec -p @rawrs/borland-c-2 tlink

Turbo Link  Version 4.0 Copyright (c) 1991 Borland International
Syntax: TLINK objfiles, exefile, mapfile, libfiles, deffile
@xxxx indicates use response file xxxx
Options: /m = map file with publics
         /x = no map file at all
         /i = initialize all segments
         /l = include source line numbers
         /L = specify library search paths
         /s = detailed map of segments
         /n = no default libraries
         /d = warn if duplicate symbols in libraries
         /c = lower case significant in symbols
         /3 = enable 32-bit processing
         /v = include full symbolic debug information
         /e = ignore Extended Dictionary
         /t = create COM file (same as /Tc)
         /o = overlay switch
         /P[=NNNNN] = pack code segments
         /A=NNNN = set NewExe segment alignment factor
         /ye = expanded memory swapping
         /yx = extended memory swapping
         /C  = case sensitive exports and imports
         /Txx = specify output file type
               /Tdx = DOS image (default)
               /Twx = Windows image
                (third letter can be c=COM, e=EXE, d=DLL)
```

For a simple case, where we used the small memory model (-ms) when compiling, we can link our MAIN.OBJ to both the C runtime for small ("S") memory (C0S.OBJ) and the C standard library (CS.LIB) via:

```
npm exec -p @rawrs/borland-c-2 tlink C0S MAIN.OBJ,MAIN.EXE,,CS
```

To link more objects, specify them together... and to link more libraries do the same for them at the end:

```
npm exec -p @rawrs/borland-c-2 tlink C0S MAIN.OBJ ANOTHER.OBJ,MAIN.EXE,,CS MATHS
```

You can also directly invoke the assembler, TASM, via:

```
npm exec -p @rawrs/borland-c-2 tasm
```
