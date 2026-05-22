struct S3 { int a; char b; };
int extract(struct S3 s) {
  return s.a + s.b;
}
