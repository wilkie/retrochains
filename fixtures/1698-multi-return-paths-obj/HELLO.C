int sign(int n) {
  if (n > 0) return 1;
  if (n < 0) return -1;
  return 0;
}
int main(void) {
  return sign(-5) + sign(0) + sign(7);
}
