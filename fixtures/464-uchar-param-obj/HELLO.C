int g;
int f(unsigned char c) {
  g = c + 1;
  return 0;
}
int main(void) {
  f(200);
  return 0;
}
