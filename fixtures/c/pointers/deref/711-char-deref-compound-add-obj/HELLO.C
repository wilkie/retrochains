char g;
int main() {
  char *p;
  p = &g;
  *p += 5;
  return *p;
}
