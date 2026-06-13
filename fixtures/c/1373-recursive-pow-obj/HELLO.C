int rpow(int b, int e) {
  if (e == 0) return 1;
  return b * rpow(b, e - 1);
}
int main(void) {
  return rpow(2, 5);
}
