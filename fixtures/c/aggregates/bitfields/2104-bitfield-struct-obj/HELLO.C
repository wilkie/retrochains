struct Bits { unsigned a : 3; unsigned b : 5; unsigned c : 8; };
int main(void) {
  struct Bits bf;
  bf.a = 5;
  bf.b = 20;
  bf.c = 100;
  return (int)bf.a + (int)bf.b + (int)bf.c;
}
