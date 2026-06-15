struct Q { unsigned int port; };
struct Q q;
int get(void) {
  return q.port;
}
