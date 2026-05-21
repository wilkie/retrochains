struct Bits {
  unsigned int a : 4;
  unsigned int b : 4;
  unsigned int c : 8;
};
int main(void) {
  struct Bits b;
  b.a = 0x3;
  b.b = 0x5;
  b.c = 0x7F;
  return (int)(b.a + b.b + b.c);
}
