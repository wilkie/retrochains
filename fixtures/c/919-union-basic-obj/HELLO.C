union U { int i; char c[2]; };
union U u;
int main() {
  u.i = 0x4142;
  return u.c[0];
}
