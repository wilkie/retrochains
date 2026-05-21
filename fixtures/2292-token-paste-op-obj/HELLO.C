#define CAT(a, b) a##b
int main(void) {
  int xy = 99;
  return CAT(x, y);
}
