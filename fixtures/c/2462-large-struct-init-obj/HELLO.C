struct Big {
  int a;
  int b;
  int c;
  int d;
  int e;
};
struct Big big = {10, 20, 30, 40, 50};
int main(void) {
  return big.a + big.c + big.e;
}
