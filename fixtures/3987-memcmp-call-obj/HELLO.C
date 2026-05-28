int memcmp(void *, void *, unsigned);
int main(void) {
  char a[3] = { 1, 2, 3 };
  char b[3] = { 1, 2, 4 };
  return memcmp(a, b, 3);
}
