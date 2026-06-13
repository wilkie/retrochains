union U { int as_int; char as_bytes[2]; };
int main(void) {
  union U u;
  u.as_int = 0x4142;
  return u.as_bytes[0] + u.as_bytes[1];
}
