char g;
int main() {
  char *p;
  p = &g;
  (*p)--;
  return *p;
}
