int pow(int b, int e) {
  int r = 1;
  int i;
  for (i = 0; i < e; i++) r *= b;
  return r;
}
int main(void) {
  return pow(2, 5);
}
